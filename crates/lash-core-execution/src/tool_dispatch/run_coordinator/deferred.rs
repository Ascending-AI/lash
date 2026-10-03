//! Root ownership and retained-result acceptance for Deferred calls.
use super::*;

impl<'a> RunCoordinator<'a> {
    /// Wait at the Run for the real seals of every open Deferred call.
    /// Dispatch descriptors take no rank and never reach presentation.
    /// A resolution that won before cancellation remains protected.
    ///
    /// # Errors
    /// Journal, source authority and typed retained-material refusals. A
    /// handover leaves every call open for the successor's same source.
    pub async fn await_deferred(&mut self) -> Result<(), SingletonRunError> {
        use crate::tool_run::SourceSubscription;
        while !self.waiting.is_empty() {
            let ids: Vec<_> = self.waiting.keys().cloned().collect();
            let cancelling = ids
                .iter()
                .any(|id| self.waiting[id].handlers.get().run_cancel_requested());
            let selected = if cancelling {
                let id = &ids[0];
                (
                    0,
                    self.journal
                        .scoped
                        .controller()
                        .cancel_run_source(self.sources[id].clone())
                        .await?,
                )
            } else {
                let subscriptions = ids
                    .iter()
                    .map(|id| SourceSubscription {
                        source: self.sources[id].source.clone(),
                        owner: self.sources[id].owner.clone(),
                        segment: self.journal.segment,
                    })
                    .collect();
                let cancel = self
                    .journal
                    .scoped
                    .turn_cancel_wait(tokio_util::sync::CancellationToken::new());
                match self
                    .journal
                    .scoped
                    .controller()
                    .await_run_sources(subscriptions, cancel)
                    .await
                {
                    Ok(selected) => selected,
                    Err(error)
                        if error.code
                            == crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled =>
                    {
                        // The gate is a request. Each source's reply decides
                        // whether its real result already won.
                        for id in &ids {
                            let seal = self
                                .journal
                                .scoped
                                .controller()
                                .cancel_run_source(self.sources[id].clone())
                                .await?;
                            self.accept_source(id, seal).await?;
                        }
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            let id = ids.get(selected.0).ok_or_else(|| boundary(&ids[0]))?;
            self.accept_source(id, selected.1).await?;
        }
        Ok(())
    }

    pub(super) async fn accept_source(
        &mut self,
        id: &ToolCallId,
        seal: crate::tool_run::SourceSeal,
    ) -> Result<(), SingletonRunError> {
        use crate::tool_run::{MaterialHolder, SourceSeal};
        let waiting = self.waiting.remove(id).ok_or_else(|| boundary(id))?;
        match seal {
            SourceSeal::Cancelled => {
                let rank = self.journal.ledger.next_rank();
                let record = self.journal.record(vec![RunEvent::Decided {
                    call_id: id.clone(),
                    rank,
                    decision: CallDecision::Cancelled,
                    after: None,
                }]);
                self.journal
                    .append(
                        record_name(id, "decide"),
                        Box::pin(async move {
                            Ok(RunJournalEntry {
                                record,
                                materials: Vec::new(),
                                state: Vec::new(),
                            })
                        }),
                    )
                    .await?;
                self.owed.insert(
                    rank,
                    Owed {
                        call_id: id.clone(),
                        handlers: waiting.handlers,
                        decision: CallDecision::Cancelled,
                        capture: None,
                    },
                );
            }
            SourceSeal::Resolved { result } => {
                let source = &self.sources[id];
                let store = waiting
                    .handlers
                    .get()
                    .tool_material_store()
                    .ok_or_else(|| {
                        RuntimeEffectControllerError::from(MaterialRefusal::Missing {
                            reference: result.clone(),
                        })
                    })?;
                let payload = store
                    .read_material(
                        &MaterialHolder::Source {
                            source: source.source.clone(),
                        },
                        &result,
                        &MaterialOwner::Source {
                            source: source.source.clone(),
                        },
                        &self.journal.materials.available,
                    )
                    .await?;
                let capture: SingletonCapture =
                    serde_json::from_str(&payload.text).map_err(|error| {
                        RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RecordEncodingFailed,
                            format!("the source's retained result does not decode: {error}"),
                        )
                    })?;
                let shape = OutcomeShape::Done {
                    intents: capture.intents(),
                };
                let capture = match waiting.member.declaration.admits(shape) {
                    Ok(()) => capture,
                    Err(refusal) => SingletonCapture::Refused { refusal },
                };
                // Source bytes stay canonical at the retained source; the
                // decision carries its reference, never another payload copy.
                self.journal
                    .materials
                    .entries
                    .insert(*result.clone(), Some(payload));
                self.decide_candidate(
                    &waiting.call,
                    waiting.handlers,
                    &waiting.member,
                    Some((
                        ResultSource::DeferredCompletion {
                            attempt: waiting.attempt,
                            resolved: result,
                        },
                        capture,
                    )),
                )
                .await?;
            }
        }
        Ok(())
    }
}
