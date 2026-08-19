use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn leading_command_claim(&mut self, phase: RuntimeLeadingCommandClaimPhase) {
        self.ensure_lease().await;
        let (owner, lease) = self.owner_and_lease();
        if let Some(expected) = phase.turn_claim_blocked_by_command {
            let blocked_turn = self
                .store()
                .claim_ready_queued_work(
                    self.session_id,
                    &lease.fence(),
                    owner,
                    QueuedWorkClaimBoundary::Idle,
                    crate::testing::queued_work_claim_policy(10),
                )
                .await
                .expect("turn claim before command")
                .claim();
            assert_eq!(
                blocked_turn.is_none(),
                expected,
                "{} leading command gate expectation changed",
                self.name
            );
        }

        let command_claim = self
            .store()
            .claim_leading_ready_session_command(self.session_id, &lease.fence(), owner)
            .await
            .expect("claim leading session command");
        assert_eq!(
            command_claim
                .as_ref()
                .map(|claim| claim.batches.len())
                .unwrap_or_default(),
            phase.expected_count,
            "{} claimed session command count changed",
            self.name
        );
        self.command_claim = command_claim;
    }

    pub(super) async fn turn_work_claim(&mut self, phase: RuntimeTurnWorkClaimPhase) {
        self.ensure_lease().await;
        let (owner, lease) = self.owner_and_lease();
        let turn_claim = self
            .store()
            .claim_ready_queued_work(
                self.session_id,
                &lease.fence(),
                owner,
                phase.boundary,
                crate::testing::queued_work_claim_policy(10),
            )
            .await
            .expect("claim ready turn work")
            .claim();
        assert_eq!(
            turn_claim
                .as_ref()
                .map(|claim| claim.batches.len())
                .unwrap_or_default(),
            phase.expected_count,
            "{} claimed turn-work count changed",
            self.name
        );
        if !phase.pending_turn_inputs_after_queue_claim.is_empty() {
            assert_pending_turn_inputs(
                self.name,
                self.store(),
                self.session_id,
                &self.enqueued_turn_inputs,
                &phase.pending_turn_inputs_after_queue_claim,
            )
            .await;
        }
        self.turn_claim = turn_claim;
    }

    pub(super) async fn next_turn_input_claim(&mut self, phase: RuntimeNextTurnInputClaimPhase) {
        self.ensure_lease().await;
        if phase.expected_aliases.len() != phase.expected_texts.len() {
            panic!(
                "{} next-turn input claim expected aliases and texts must align",
                self.name
            );
        }
        let (owner, lease) = self.owner_and_lease();
        let claim = self
            .store()
            .claim_next_turn_inputs(self.session_id, &lease.fence(), owner, 10)
            .await
            .unwrap_or_else(|err| panic!("{} failed to claim next-turn inputs: {err}", self.name));
        if phase.expected_aliases.is_empty() {
            assert!(
                claim.is_none(),
                "{} did not expect a next-turn input claim",
                self.name
            );
            return;
        }
        let claim =
            claim.unwrap_or_else(|| panic!("{} expected a next-turn input claim", self.name));
        assert_eq!(
            claim
                .inputs
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
                                "{} expected unknown claimed turn-input alias `{alias}`",
                                self.name
                            )
                        })
                        .input_id
                        .as_str()
                })
                .collect::<Vec<_>>(),
            "{} claimed next-turn input ids changed",
            self.name
        );
        assert_eq!(
            claim
                .inputs
                .iter()
                .filter_map(pending_input_text)
                .collect::<Vec<_>>(),
            phase.expected_texts,
            "{} claimed next-turn input payloads changed",
            self.name
        );
        if phase.pending_turn_inputs_hidden_after_claim {
            assert!(
                self.store()
                    .list_pending_turn_inputs(self.session_id)
                    .await
                    .unwrap_or_else(|err| panic!(
                        "{} failed to list pending turn inputs after claim: {err}",
                        self.name
                    ))
                    .is_empty(),
                "{} live claimed turn inputs should be hidden from queue preview",
                self.name
            );
        }
        self.turn_input_claim = Some(claim);
    }
}
