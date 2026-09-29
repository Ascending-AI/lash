use super::*;

/// The root every scenario's admissions bind their rows to.
pub(super) const SCENARIO_ROOT: &str = "runtime-scenario-root";

impl RuntimeScenarioContext {
    /// The session's earliest open turn-lane batch, the head a batch-headed
    /// root is admitted on.
    async fn turn_lane_head(&self) -> Option<lash_core::BatchId> {
        self.store()
            .list_open_queued_work(&self.session_id)
            .await
            .expect("list open queued work")
            .into_iter()
            .filter(|batch| batch.work_class() == QueuedWorkClass::TurnWork)
            .min_by_key(|batch| batch.enqueue_seq)
            .map(|batch| batch.batch_id)
    }

    /// Admit the scenario's root headed by `head`; `None` when the admission
    /// cannot reach it.
    async fn admit_scenario_root(&self, head: AdmittedHead) -> Option<RootAdmission> {
        let (_, fence) = self.owner_and_lease();
        let mut request = lash_core::testing::store_fixtures::admit_root_request_for_test(
            fence,
            &TurnId::from(SCENARIO_ROOT),
            head,
        );
        request.policy = lash_core::testing::queued_work_admission_policy(10);
        request.max_inputs = 10;
        self.store()
            .admit_root(&request)
            .await
            .unwrap_or_else(|err| panic!("{} failed to admit its root: {err}", self.name))
    }

    pub(super) async fn leading_command_claim(&mut self, phase: RuntimeLeadingCommandClaimPhase) {
        self.ensure_lease().await;
        if let Some(expected) = phase.turn_claim_blocked_by_command {
            let head = self
                .turn_lane_head()
                .await
                .expect("the command gate expectation needs turn work behind it");
            let blocked_turn = self.admit_scenario_root(AdmittedHead::Batch(head)).await;
            assert_eq!(
                blocked_turn.is_none(),
                expected,
                "{} leading command gate expectation changed",
                self.name
            );
        }

        let (_, fence) = self.owner_and_lease();
        let commands = self
            .store()
            .open_session_command_run(fence)
            .await
            .expect("open the leading session-command run");
        assert_eq!(
            commands.len(),
            phase.expected_count,
            "{} session-command run length changed",
            self.name
        );
        self.commands = commands;
    }

    pub(super) async fn turn_work_claim(&mut self, phase: RuntimeTurnWorkClaimPhase) {
        self.ensure_lease().await;
        let batches = match phase.boundary {
            AdmissionBoundary::Idle => match self.turn_lane_head().await {
                Some(head) => {
                    let admission = self
                        .admit_scenario_root(AdmittedHead::Batch(head))
                        .await
                        .unwrap_or_else(|| panic!("{} root admission missed its head", self.name));
                    let batches = admission.batch_ids();
                    self.admission = Some(admission);
                    batches
                }
                None => Vec::new(),
            },
            AdmissionBoundary::ActiveTurnCheckpoint => {
                let (_, fence) = self.owner_and_lease();
                let root = TurnId::from(SCENARIO_ROOT);
                let admission = self
                    .store()
                    .admit_at_checkpoint(&lash_core::store::CheckpointAdmissionRequest {
                        fence: fence.clone(),
                        root: root.clone(),
                        turn_id: root,
                        checkpoint: lash_core::CheckpointKind::AfterWork,
                        step: "runtime-scenario-checkpoint".to_string(),
                        max_inputs: 10,
                        policy: lash_core::testing::queued_work_admission_policy(10),
                    })
                    .await
                    .unwrap_or_else(|err| {
                        panic!("{} failed to admit at its checkpoint: {err}", self.name)
                    });
                let batches = admission
                    .queued
                    .iter()
                    .flat_map(|queued| queued.batch_ids())
                    .collect::<Vec<_>>();
                self.checkpoint_admission = Some(admission);
                batches
            }
        };
        assert_eq!(
            batches.len(),
            phase.expected_count,
            "{} admitted turn-work count changed",
            self.name
        );
        if !phase.pending_turn_inputs_after_queue_claim.is_empty() {
            assert_pending_turn_inputs(
                self.name,
                self.store(),
                &self.session_id,
                &self.enqueued_turn_inputs,
                &phase.pending_turn_inputs_after_queue_claim,
            )
            .await;
        }
    }

    pub(super) async fn next_turn_input_claim(
        &mut self,
        phase: RuntimeNextTurnInputAdmissionPhase,
    ) {
        self.ensure_lease().await;
        if phase.expected_aliases.len() != phase.expected_texts.len() {
            panic!(
                "{} next-turn input admission expected aliases and texts must align",
                self.name
            );
        }
        let head = self.next_turn_input_head().await;
        if phase.expected_aliases.is_empty() {
            assert!(
                head.is_none(),
                "{} did not expect an admissible next-turn input",
                self.name
            );
            return;
        }
        let head =
            head.unwrap_or_else(|| panic!("{} expected an admissible next-turn input", self.name));
        let admission = self
            .admit_scenario_root(AdmittedHead::Input(head))
            .await
            .unwrap_or_else(|| panic!("{} root admission missed its input head", self.name));
        let inputs = admission
            .inputs
            .as_ref()
            .map(|admitted| admitted.inputs.clone())
            .unwrap_or_default();
        assert_eq!(
            inputs
                .iter()
                .map(|input| input.input_id.as_str())
                .collect::<Vec<_>>(),
            phase
                .expected_aliases
                .iter()
                .map(|alias| {
                    self.enqueued_turn_inputs
                        .get(alias)
                        .unwrap_or_else(|| {
                            panic!(
                                "{} expected unknown admitted turn-input alias `{alias}`",
                                self.name
                            )
                        })
                        .input_id
                        .as_str()
                })
                .collect::<Vec<_>>(),
            "{} admitted next-turn input ids changed",
            self.name
        );
        assert_eq!(
            inputs
                .iter()
                .filter_map(pending_input_text)
                .collect::<Vec<_>>(),
            phase.expected_texts,
            "{} admitted next-turn input payloads changed",
            self.name
        );
        if phase.verify_pending_turn_inputs_held_after_claim {
            let reads = self
                .store()
                .list_pending_turn_inputs(&self.session_id)
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "{} failed to list pending turn inputs after admission: {err}",
                        self.name
                    )
                });
            assert_eq!(
                reads
                    .iter()
                    .map(|read| read.input.input_id.as_str())
                    .collect::<Vec<_>>(),
                inputs
                    .iter()
                    .map(|input| input.input_id.as_str())
                    .collect::<Vec<_>>(),
                "{} admitted turn inputs must remain visible",
                self.name
            );
            assert!(
                reads.iter().all(|read| {
                    read.status
                        == lash_core::PendingTurnInputReadStatus::Admitted {
                            root: TurnId::from(SCENARIO_ROOT),
                        }
                }),
                "{} admitted turn inputs must report their root",
                self.name
            );
        }
        self.admission = Some(admission);
    }

    /// The session's earliest open next-turn input.
    pub(super) async fn next_turn_input_head(&self) -> Option<lash_core::InputId> {
        self.store()
            .list_pending_turn_inputs(&self.session_id)
            .await
            .unwrap_or_else(|err| panic!("{} failed to list pending inputs: {err}", self.name))
            .into_iter()
            .find(|read| {
                matches!(read.status, lash_core::PendingTurnInputReadStatus::Open)
                    && matches!(read.input.ingress(), TurnInputIngress::NextTurn)
            })
            .map(|read| read.input.input_id)
    }
}
