use super::*;

/// The run every scenario's admissions bind their rows to.
pub(super) const SCENARIO_RUN: &str = "runtime-scenario-run";

impl RuntimeScenarioContext {
    /// The session's earliest open turn-lane batch, the head a batch-headed
    /// run is admitted on.
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

    /// Admit the scenario's run headed by `head`; `None` when the admission
    /// cannot reach it.
    pub(super) async fn admit_scenario_run(&self, head: AdmittedHead) -> Option<RunAdmission> {
        let (_, fence) = self.owner_and_lease();
        let mut request = lash_core::testing::store_fixtures::admit_run_request_for_test(
            fence,
            &TurnId::from(SCENARIO_RUN),
            head,
        );
        request.policy = lash_core::testing::queued_work_admission_policy(10);
        request.max_inputs = 10;
        self.store()
            .admit_run(&request)
            .await
            .unwrap_or_else(|err| panic!("{} failed to admit its run: {err}", self.name))
    }

    pub(super) async fn leading_command_run(&mut self, phase: RuntimeLeadingCommandRunPhase) {
        self.ensure_lease().await;
        if let Some(expected) = phase.turn_admission_blocked_by_command {
            let head = self
                .turn_lane_head()
                .await
                .expect("the command gate expectation needs turn work behind it");
            let blocked_turn = self.admit_scenario_run(AdmittedHead::Batch(head)).await;
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

    pub(super) async fn turn_work_admission(&mut self, phase: RuntimeTurnWorkAdmissionPhase) {
        self.ensure_lease().await;
        let batches = match phase.boundary {
            AdmissionBoundary::Idle => match self.turn_lane_head().await {
                Some(head) => {
                    let admission = self
                        .admit_scenario_run(AdmittedHead::Batch(head))
                        .await
                        .unwrap_or_else(|| panic!("{} run admission missed its head", self.name));
                    let batches = admission.batch_ids();
                    self.admission = Some(admission);
                    batches
                }
                None => Vec::new(),
            },
            AdmissionBoundary::ActiveTurnCheckpoint => {
                let (_, fence) = self.owner_and_lease();
                let run = TurnId::from(SCENARIO_RUN);
                let admission = self
                    .store()
                    .admit_at_checkpoint(&lash_core::store::CheckpointAdmissionRequest {
                        fence: fence.clone(),
                        run: run.clone(),
                        turn_id: run,
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
        if !phase.pending_turn_inputs_after_queue_admission.is_empty() {
            assert_pending_turn_inputs(
                self.name,
                self.store(),
                &self.session_id,
                &self.enqueued_turn_inputs,
                &phase.pending_turn_inputs_after_queue_admission,
            )
            .await;
        }
    }

    pub(super) async fn next_turn_input_admission(
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
            .admit_scenario_run(AdmittedHead::Input(head))
            .await
            .unwrap_or_else(|| panic!("{} run admission missed its input head", self.name));
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
        if phase.verify_pending_turn_inputs_held_after_admission {
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
                            run: TurnId::from(SCENARIO_RUN),
                        }
                }),
                "{} admitted turn inputs must report their run",
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
                    && read.input.state.is_next_turn_input(
                        self.admission
                            .as_ref()
                            .map(|_| TurnId::from(SCENARIO_RUN))
                            .as_ref(),
                    )
            })
            .map(|read| read.input.input_id)
    }
}
