//! Short protected-work admission frames and independent receipt registration.
use super::*;
use futures_util::FutureExt;

impl<'a> RunCoordinator<'a> {
    pub(super) fn register_realization(
        &mut self,
        call_id: ToolCallId,
        receipt: crate::tool_dispatch::RunSelectable<'a, RealizationReceipt>,
    ) {
        let handle = async move {
            Ok(parallel::Ready::Realization(std::sync::Arc::new(
                receipt.value.await?,
            )))
        }
        .boxed()
        .shared();
        self.realizing.insert(
            call_id.clone(),
            parallel::Realizing {
                key: receipt.key.shared(),
                handle,
            },
        );
    }

    /// Admit protected declarations at the open drain frontier without waiting
    /// for external realization or presenting a result. A physical cut can
    /// then transfer the durable invocation reference while its work continues.
    ///
    /// # Errors
    /// A typed admission, material or journal refusal.
    pub async fn begin_drain(&mut self) -> Result<(), SingletonRunError> {
        self.begin_frame()?;
        let result = self.bodies.clone().beside(self.begin_drain_inner()).await;
        self.active_frame = false;
        self.note_fault(&result);
        result
    }

    async fn begin_drain_inner(&mut self) -> Result<(), SingletonRunError> {
        let ready: Vec<_> = self
            .owed
            .iter()
            .filter(|(rank, _)| self.journal.ledger.drain_frontier_open(**rank))
            .map(|(rank, owed)| {
                (
                    *rank,
                    owed.call_id.clone(),
                    owed.capture.clone(),
                    owed.decision.clone(),
                    owed.handlers.clone(),
                )
            })
            .collect();
        for (rank, call_id, capture, decision, handlers) in ready {
            if let (CallDecision::Final { declares: true, .. }, Some(capture)) = (decision, capture)
            {
                self.admit_declarations(rank, call_id, &capture, true, handlers.get())
                    .await?;
            }
        }
        Ok(())
    }
}
