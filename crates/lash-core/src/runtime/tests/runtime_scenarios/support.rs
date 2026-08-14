use super::*;
pub(crate) use std::collections::HashMap;

pub(crate) use crate::store::{
    QueuedWorkStore, SessionCommitStore, SessionExecutionLeaseStore, TurnInputStore,
};
pub(crate) use crate::{
    LeaseOwnerIdentity, PendingTurnInput, PendingTurnInputDraft, RuntimeCommit,
    SessionExecutionLease, SessionExecutionLeaseClaimOutcome, StoreError, TurnInput,
    TurnInputCheckpointBoundary, TurnInputClaim, TurnInputIngress, TurnInputState,
};
pub(crate) use helpers::RecordingStore;

#[path = "support/checkpoint.rs"]
mod checkpoint;
#[path = "support/claim.rs"]
mod claim;
#[path = "support/commit.rs"]
mod commit;
#[path = "support/fault.rs"]
mod fault;
#[path = "support/ingress.rs"]
mod ingress;
#[path = "support/lease.rs"]
mod lease;

#[derive(Clone, Debug)]
pub(crate) struct RuntimeScenario {
    pub(crate) name: &'static str,
    pub(crate) session_id: &'static str,
    pub(crate) host_behavior: RuntimeHostBehavior,
    pub(crate) phases: Vec<RuntimeScenarioPhase>,
}
impl RuntimeScenario {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            session_id: "root",
            host_behavior: RuntimeHostBehavior::default(),
            phases: Vec::new(),
        }
    }

    pub(crate) fn session_id(mut self, session_id: &'static str) -> Self {
        self.session_id = session_id;
        self
    }

    pub(crate) fn host_behavior(mut self, host_behavior: RuntimeHostBehavior) -> Self {
        self.host_behavior = host_behavior;
        self
    }

    pub(crate) fn phase(mut self, phase: impl Into<RuntimeScenarioPhase>) -> Self {
        self.phases.push(phase.into());
        self
    }

    pub(crate) async fn run(self) {
        assert!(
            !self.phases.is_empty(),
            "{} must declare at least one RuntimeScenario phase",
            self.name
        );
        self.validate_phase_order();
        let mut context =
            RuntimeScenarioContext::new(self.name, self.session_id, self.host_behavior);
        for phase in self.phases {
            context.execute(phase).await;
        }
    }

    fn validate_phase_order(&self) {
        let mut saw_live_lease_claim = false;
        let mut saw_turn_work_claim = false;
        for (index, phase) in self.phases.iter().enumerate() {
            if RuntimeScenarioPhase::releases_session_lease(phase) && index + 1 != self.phases.len()
            {
                panic!(
                    "{} declares a lease-releasing phase before the final phase",
                    self.name
                );
            }
            if RuntimeScenarioPhase::requires_live_session_lease(phase) {
                saw_live_lease_claim = true;
            }
            match phase {
                RuntimeScenarioPhase::Lease(RuntimeLeasePhase::ExpireStaleHolder { .. })
                    if saw_live_lease_claim =>
                {
                    panic!(
                        "{} stale-holder expiry must be declared before lease-claiming phases",
                        self.name
                    );
                }
                RuntimeScenarioPhase::TurnWorkClaim(_) => {
                    saw_live_lease_claim = true;
                    saw_turn_work_claim = true;
                }
                RuntimeScenarioPhase::Lease(RuntimeLeasePhase::ExpireStaleHolder { .. }) => {
                    saw_live_lease_claim = true;
                }
                RuntimeScenarioPhase::Fault(RuntimeFaultPhase::StaleQueueCompletion)
                    if !saw_turn_work_claim =>
                {
                    panic!(
                        "{} stale queue-completion fault requires a prior turn-work claim phase",
                        self.name
                    );
                }
                RuntimeScenarioPhase::NextTurnInputClaim(phase)
                    if phase.expected_aliases.len() != phase.expected_texts.len() =>
                {
                    panic!(
                        "{} next-turn input claim expected aliases and texts must align",
                        self.name
                    );
                }
                _ => {}
            }
        }
    }

    pub(crate) fn phase_order_is_valid_for_test(&self) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.validate_phase_order()))
            .is_ok()
    }
}

struct RuntimeScenarioContext {
    name: &'static str,
    session_id: &'static str,
    host_behavior: RuntimeHostBehavior,
    clock: Arc<crate::testing::TestClock>,
    store: Arc<RecordingStore>,
    owner: Option<LeaseOwnerIdentity>,
    lease: Option<SessionExecutionLease>,
    state: RuntimeSessionState,
    enqueued_turn_inputs: HashMap<&'static str, PendingTurnInput>,
    command_claim: Option<QueuedWorkClaim>,
    turn_claim: Option<QueuedWorkClaim>,
    turn_input_claim: Option<TurnInputClaim>,
    lease_released: bool,
}

impl RuntimeScenarioContext {
    fn new(
        name: &'static str,
        session_id: &'static str,
        host_behavior: RuntimeHostBehavior,
    ) -> Self {
        let mut state = RuntimeSessionState {
            session_id: session_id.to_string(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        };
        state.ensure_agent_frame_initialized();
        let clock = Arc::new(crate::testing::TestClock::new(10_000));
        Self {
            name,
            session_id,
            host_behavior,
            store: Arc::new(RecordingStore::with_clock(clock.clone())),
            clock,
            owner: None,
            lease: None,
            state,
            enqueued_turn_inputs: HashMap::new(),
            command_claim: None,
            turn_claim: None,
            turn_input_claim: None,
            lease_released: false,
        }
    }

    async fn execute(&mut self, phase: RuntimeScenarioPhase) {
        self.store
            .admit_and_bind_session(&crate::SessionBinding::root(self.session_id))
            .await
            .expect("bind runtime scenario session");
        match phase {
            RuntimeScenarioPhase::Ingress(phase) => self.ingress(phase).await,
            RuntimeScenarioPhase::Checkpoint(phase) => self.checkpoint(phase).await,
            RuntimeScenarioPhase::LeadingCommandClaim(phase) => {
                self.leading_command_claim(phase).await
            }
            RuntimeScenarioPhase::TurnWorkClaim(phase) => self.turn_work_claim(phase).await,
            RuntimeScenarioPhase::NextTurnInputClaim(phase) => {
                self.next_turn_input_claim(phase).await
            }
            RuntimeScenarioPhase::Lease(phase) => self.lease_phase(phase).await,
            RuntimeScenarioPhase::Fault(phase) => self.fault(phase).await,
            RuntimeScenarioPhase::Commit(phase) => self.commit(phase).await,
        }
    }

    fn store(&self) -> &RecordingStore {
        self.store.as_ref()
    }

    async fn ensure_lease(&mut self) {
        if self.lease_released {
            panic!(
                "{} declared a phase requiring a session lease after the lease was released",
                self.name
            );
        }
        if self.lease.is_some() {
            return;
        }
        let owner = lease_owner(self.host_behavior.lease_owner_id);
        let lease = self
            .store()
            .try_claim_session_execution_lease(
                self.session_id,
                &owner,
                "ensure-lease-executor",
                60_000,
            )
            .await
            .expect("claim session execution lease")
            .acquired()
            .expect("session execution lease");
        self.owner = Some(owner);
        self.lease = Some(lease);
    }

    fn owner_and_lease(&self) -> (&LeaseOwnerIdentity, &SessionExecutionLease) {
        (
            self.owner
                .as_ref()
                .expect("RuntimeScenario phase forgot to claim an owner"),
            self.lease
                .as_ref()
                .expect("RuntimeScenario phase forgot to claim a lease"),
        )
    }
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
    LeadingCommandClaim(RuntimeLeadingCommandClaimPhase),
    TurnWorkClaim(RuntimeTurnWorkClaimPhase),
    NextTurnInputClaim(RuntimeNextTurnInputClaimPhase),
    Lease(RuntimeLeasePhase),
    Fault(RuntimeFaultPhase),
    Commit(RuntimeCommitPhase),
}

impl RuntimeScenarioPhase {
    fn requires_live_session_lease(&self) -> bool {
        matches!(
            self,
            Self::Checkpoint(_)
                | Self::LeadingCommandClaim(_)
                | Self::TurnWorkClaim(_)
                | Self::NextTurnInputClaim(_)
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
    pub(crate) no_next_turn_input_claim_after_cancellations: bool,
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

    pub(crate) fn expect_no_next_turn_input_claim_after_cancellations(mut self) -> Self {
        self.no_next_turn_input_claim_after_cancellations = true;
        self
    }
}

impl From<RuntimeCheckpointPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeCheckpointPhase) -> Self {
        Self::Checkpoint(phase)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeLeadingCommandClaimPhase {
    pub(crate) expected_count: usize,
    pub(crate) turn_claim_blocked_by_command: Option<bool>,
}

impl RuntimeLeadingCommandClaimPhase {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn expect_count(mut self, count: usize) -> Self {
        self.expected_count = count;
        self
    }

    pub(crate) fn expect_turn_work_blocked_before_command(mut self, blocked: bool) -> Self {
        self.turn_claim_blocked_by_command = Some(blocked);
        self
    }
}

impl From<RuntimeLeadingCommandClaimPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeLeadingCommandClaimPhase) -> Self {
        Self::LeadingCommandClaim(phase)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeTurnWorkClaimPhase {
    pub(crate) boundary: QueuedWorkClaimBoundary,
    pub(crate) expected_count: usize,
    pub(crate) pending_turn_inputs_after_queue_claim: Vec<RuntimePendingTurnInputExpectation>,
}

impl RuntimeTurnWorkClaimPhase {
    pub(crate) fn at(boundary: QueuedWorkClaimBoundary) -> Self {
        Self {
            boundary,
            expected_count: 0,
            pending_turn_inputs_after_queue_claim: Vec::new(),
        }
    }

    pub(crate) fn expect_count(mut self, count: usize) -> Self {
        self.expected_count = count;
        self
    }

    pub(crate) fn expect_pending_turn_inputs_after_claim(
        mut self,
        expectations: Vec<RuntimePendingTurnInputExpectation>,
    ) -> Self {
        self.pending_turn_inputs_after_queue_claim = expectations;
        self
    }
}

impl From<RuntimeTurnWorkClaimPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeTurnWorkClaimPhase) -> Self {
        Self::TurnWorkClaim(phase)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeNextTurnInputClaimPhase {
    pub(crate) expected_aliases: Vec<&'static str>,
    pub(crate) expected_texts: Vec<&'static str>,
    pub(crate) pending_turn_inputs_hidden_after_claim: bool,
}

impl RuntimeNextTurnInputClaimPhase {
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

    pub(crate) fn expect_pending_hidden_after_claim(mut self) -> Self {
        self.pending_turn_inputs_hidden_after_claim = true;
        self
    }
}

impl From<RuntimeNextTurnInputClaimPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeNextTurnInputClaimPhase) -> Self {
        Self::NextTurnInputClaim(phase)
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
    pub(crate) fn batch_draft(&self, session_id: &str) -> QueuedWorkBatchDraft {
        match self {
            Self::RefreshToolCatalog { reason } => QueuedWorkBatchDraft::new(
                session_id,
                DeliveryPolicy::EarliestSafeBoundary,
                vec![QueuedWorkPayload::session_command(
                    SessionCommand::RefreshToolCatalog {
                        reason: (*reason).to_string(),
                    },
                )],
            ),
            Self::ProcessWake { text } => crate::process_wake_batch_draft(ProcessWakeDelivery {
                version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
                wake_id: format!("wake:{session_id}:{text}"),
                target_session_id: session_id.to_string(),
                process_id: format!("process:{text}"),
                sequence: 1,
                event_type: "process.wake".to_string(),
                event_invocation: RuntimeInvocation {
                    scope: RuntimeScope::new(session_id),
                    subject: RuntimeSubject::ProcessEvent {
                        process_id: format!("process:{text}"),
                        sequence: 1,
                        event_type: "process.wake".to_string(),
                    },
                    caused_by: None,
                    replay: None,
                },
                process_caused_by: None,
                authority: crate::QueuedWorkAuthority::default(),
                input: (*text).to_string(),
                created_at_ms: 1,
            }),
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
        session_id: &'static str,
        text: &'static str,
    },
    ActiveTurn {
        alias: &'static str,
        turn_id: &'static str,
        min_boundary: TurnInputCheckpointBoundary,
        text: &'static str,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimePendingTurnInputExpectation {
    pub(crate) alias: &'static str,
    pub(crate) state: TurnInputState,
    pub(crate) ingress: RuntimePendingTurnInputIngressExpectation,
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimePendingTurnInputIngressExpectation {
    NextTurn,
}

pub(crate) fn lease_owner(owner_id: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

pub(crate) fn local_lease_owner(owner_id: &str, _process_start: &str) -> LeaseOwnerIdentity {
    lease_owner(owner_id)
}

pub(crate) fn pending_next_turn_input_draft(session_id: &str, text: &str) -> PendingTurnInputDraft {
    PendingTurnInputDraft::new(
        session_id,
        TurnInputIngress::NextTurn,
        TurnInput::text(text),
    )
}

pub(crate) fn pending_active_turn_input_draft(
    session_id: &str,
    turn_id: &str,
    min_boundary: TurnInputCheckpointBoundary,
    text: &str,
) -> PendingTurnInputDraft {
    PendingTurnInputDraft::new(
        session_id,
        TurnInputIngress::active_turn(turn_id, min_boundary),
        TurnInput::text(text),
    )
}

pub(crate) fn pending_input_text(input: &PendingTurnInput) -> Option<&str> {
    match input.input.items.first()? {
        InputItem::Text { text } => Some(text.as_str()),
        InputItem::Attachment { .. } => None,
    }
}

async fn assert_pending_turn_inputs(
    scenario_name: &str,
    store: &RecordingStore,
    session_id: &str,
    enqueued_turn_inputs: &HashMap<&'static str, PendingTurnInput>,
    expectations: &[RuntimePendingTurnInputExpectation],
) {
    let pending = store
        .list_pending_turn_inputs(session_id)
        .await
        .unwrap_or_else(|err| panic!("{scenario_name} failed to list pending inputs: {err}"));
    assert_eq!(
        pending
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        expectations
            .iter()
            .map(|expected| {
                enqueued_turn_inputs
                    .get(expected.alias)
                    .unwrap_or_else(|| {
                        panic!(
                            "{scenario_name} expected unknown pending turn-input alias `{}`",
                            expected.alias
                        )
                    })
                    .input_id
                    .as_str()
            })
            .collect::<Vec<_>>(),
        "{scenario_name} pending turn-input ids changed"
    );
    for (input, expected) in pending.iter().zip(expectations) {
        assert_eq!(
            input.state, expected.state,
            "{scenario_name} pending turn-input state changed for `{}`",
            expected.alias
        );
        match expected.ingress {
            RuntimePendingTurnInputIngressExpectation::NextTurn => {
                assert!(
                    matches!(input.ingress, TurnInputIngress::NextTurn),
                    "{scenario_name} expected `{}` to be pending for the next turn",
                    expected.alias
                );
            }
        }
    }
}
