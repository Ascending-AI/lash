//! Authenticate and commit early final decisions for admitted attempts.
use super::*;

impl Lifecycle {
    pub(super) fn authenticate_settlement(
        &self,
        execution: &AdmittedExecution,
        settlement: &super::super::SettlementIntent,
    ) -> Result<(), RoundError> {
        use super::super::SettlementIntent;
        let permit = match settlement {
            SettlementIntent::Nonterminal => return Ok(()),
            SettlementIntent::PlainFinal(permit)
            | SettlementIntent::ProtectedFinal(permit)
            | SettlementIntent::Cancelled { permit, .. } => permit,
        };
        if permit.authorizes(&self.cx, execution) {
            Ok(())
        } else {
            Err(RoundError::DecisionAuthority)
        }
    }

    pub(super) async fn commit_finals(
        &mut self,
        folded: &RunFold,
        attempt: Option<&lash_durable::ActorTx>,
    ) -> Result<Option<Act>, RoundError> {
        while let Ok(request) = self.final_receiver.try_recv() {
            self.final_requests
                .insert(request.permit.execution().id().clone(), request);
        }
        self.final_requests.retain(|id, request| {
            folded
                .admitted(id)
                .is_some_and(|execution| request.permit.authorizes(&self.cx, &execution))
        });
        let decided: Vec<_> = self
            .final_requests
            .keys()
            .filter_map(|id| {
                let member = folded.round(id.run)?.member_started_at(id.ordinal)?;
                (member.final_payload().is_some()
                    || member.cancellation().is_some()
                    || member.outcome().is_some())
                .then(|| (id.clone(), member.final_payload().is_some()))
            })
            .collect();
        for (id, won) in decided {
            if let Some(request) = self.final_requests.remove(&id) {
                if won {
                    request.protected.cancel();
                }
                let _ = request.reply.send(won);
            }
        }
        self.final_requests
            .retain(|_, request| !request.reply.is_closed());
        if self.final_requests.is_empty() {
            return Ok(None);
        }
        let mut tx = match attempt {
            Some(tx) => tx.clone(),
            None => self.cx.begin().await?,
        };
        // The stop may have arrived since commit_cancels read its transaction.
        self.read_host_cancels(&tx, folded);
        if self.fence.as_ref().is_some_and(|fence| fence(&tx)) {
            self.request_cancels_before(
                folded,
                RunSeq(u64::MAX),
                crate::ToolCancellation::runtime("the owning turn stopped")
                    .with_origin(crate::CancelOrigin::TurnStopped),
            );
            return Ok(Some(Act::Refused));
        }
        if self.final_requests.keys().any(|id| {
            folded
                .admitted(id)
                .is_some_and(|execution| self.cancel_requests.contains_key(execution.call()))
        }) {
            return Ok(Some(Act::Refused));
        }
        let mut staged = Vec::new();
        for (id, request) in &self.final_requests {
            if let Some(execution) = folded.admitted(id) {
                staged.push(super::super::rounds::final_call(
                    &mut tx,
                    &execution,
                    request.payload.clone(),
                ));
            }
        }
        if staged.is_empty() {
            return Ok(None);
        }
        self.commit(tx, CommitLabel::CALL_FINAL, staged)
            .await
            .map(Some)
    }
}
