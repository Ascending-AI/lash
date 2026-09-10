use super::*;

impl RuntimeScenarioContext {
    pub(super) async fn ingress(&mut self, phase: RuntimeIngressPhase) {
        let mut enqueued = Vec::new();
        for ingress in &phase.queue {
            enqueued.push(
                self.store()
                    .enqueue_queued_work(ingress.batch_draft(self.session_id))
                    .await
                    .expect("enqueue runtime scenario queued work"),
            );
        }
        assert_eq!(
            enqueued
                .iter()
                .map(|batch| batch.work_class())
                .collect::<Vec<_>>(),
            phase.enqueued_classes,
            "{} queued-work classes changed",
            self.name
        );

        for ingress in &phase.turn_inputs {
            self.enqueue_turn_input(ingress).await;
        }
        for alias in &phase.cancel_before_commit {
            self.cancel_turn_input(alias, "pending").await;
        }
    }

    async fn enqueue_turn_input(&mut self, ingress: &RuntimeTurnInputIngress) {
        match ingress {
            RuntimeTurnInputIngress::NextTurn {
                alias,
                text,
                source_key,
            } => {
                let mut draft = pending_next_turn_input_draft(self.session_id, text);
                if let Some(source_key) = source_key {
                    draft = draft.with_source_key(*source_key);
                }
                let input = self
                    .store()
                    .enqueue_pending_turn_input(draft)
                    .await
                    .unwrap_or_else(|err| {
                        panic!("{} failed to enqueue next-turn input: {err}", self.name)
                    });
                self.enqueued_turn_inputs.insert(*alias, input);
            }
            RuntimeTurnInputIngress::ReplayNextTurn {
                alias,
                text,
                source_key,
                expected_alias,
                expected_text,
            } => {
                let input = self
                    .store()
                    .enqueue_pending_turn_input(
                        pending_next_turn_input_draft(self.session_id, text)
                            .with_source_key(*source_key),
                    )
                    .await
                    .unwrap_or_else(|err| {
                        panic!("{} failed to replay source-key input: {err}", self.name)
                    });
                let expected = self
                    .enqueued_turn_inputs
                    .get(expected_alias)
                    .unwrap_or_else(|| {
                        panic!(
                            "{} source-key replay expected unknown input alias `{expected_alias}`",
                            self.name
                        )
                    });
                assert_eq!(
                    input.input_id, expected.input_id,
                    "{} source-key replay should return the original input id",
                    self.name
                );
                assert_eq!(
                    pending_input_text(&input),
                    Some(*expected_text),
                    "{} source-key replay should preserve the original payload",
                    self.name
                );
                self.enqueued_turn_inputs.insert(*alias, input);
            }
            RuntimeTurnInputIngress::ConflictNextTurnReplay {
                text,
                source_key,
                expected_alias,
            } => {
                let expected = self
                    .enqueued_turn_inputs
                    .get(expected_alias)
                    .unwrap_or_else(|| {
                        panic!(
                            "{} source-key conflict expected unknown input alias `{expected_alias}`",
                            self.name
                        )
                    });
                let err = self
                    .store()
                    .enqueue_pending_turn_input(
                        pending_next_turn_input_draft(self.session_id, text)
                            .with_source_key(*source_key),
                    )
                    .await
                    .expect_err("changed source-key replay must conflict");
                assert!(
                    matches!(
                        &err,
                        crate::StoreError::PendingTurnInputSourceKeyConflict {
                            session_id,
                            source_key: err_source_key,
                            existing_input_id,
                        } if session_id.as_str() == self.session_id
                            && err_source_key.as_str() == *source_key
                            && existing_input_id == &expected.input_id
                    ),
                    "{} changed source-key replay returned wrong error: {err}",
                    self.name
                );
            }
            RuntimeTurnInputIngress::NextTurnForSession { session_id, text } => {
                self.store()
                    .enqueue_pending_turn_input(pending_next_turn_input_draft(session_id, text))
                    .await
                    .unwrap_or_else(|err| {
                        panic!("{} failed to enqueue other-session input: {err}", self.name)
                    });
            }
            RuntimeTurnInputIngress::ActiveTurn {
                alias,
                turn_id,
                min_boundary,
                text,
            } => {
                let input = self
                    .store()
                    .enqueue_pending_turn_input(pending_active_turn_input_draft(
                        self.session_id,
                        &TurnId::from(*turn_id),
                        *min_boundary,
                        text,
                    ))
                    .await
                    .unwrap_or_else(|err| {
                        panic!("{} failed to enqueue active-turn input: {err}", self.name)
                    });
                self.enqueued_turn_inputs.insert(*alias, input);
            }
        }
    }

    pub(super) async fn cancel_turn_input(&self, alias: &'static str, label: &str) {
        let input = self.enqueued_turn_inputs.get(alias).unwrap_or_else(|| {
            panic!(
                "{} tried to cancel unknown {label} turn-input alias `{alias}`",
                self.name
            )
        });
        let outcome = self
            .store()
            .cancel_pending_turn_input(self.session_id, &input.input_id)
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "{} failed to cancel {label} turn input `{alias}`: {err}",
                    self.name
                )
            });
        let crate::PendingTurnInputCancelOutcome::Cancelled(cancelled) = outcome else {
            panic!(
                "{} expected {label} turn input `{alias}` to be cancellable, got {outcome:?}",
                self.name,
            )
        };
        assert_eq!(cancelled.input_id, input.input_id);
    }
}
