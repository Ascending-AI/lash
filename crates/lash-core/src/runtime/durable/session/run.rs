//! Run one selected turn, carrying any uncommitted admission with its context.
use super::*;
use tracing::Instrument as _;

impl SessionActivation {
    pub(super) async fn run_turn(
        &self,
        cx: &ActorContext,
        session: &SessionId,
        heads: &mut SessionOwner,
        row: TurnRow,
    ) -> Result<Pass, TurnError> {
        lash_core_ids::operational_metrics::turn::phase("owner orchestration", None, async {
            match heads.head(cx, session).await {
                Ok(_) => {}
                Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => {
                    phases::refuse(cx, self.services.tracing(), &row, refusal).await?;
                    return Ok(Pass::Again);
                }
                Err(error) => return Err(error),
            }
            // An accepted cancel request ends the turn before anything else
            // runs: the live owner stopped its work for it, or a crash left it.
            // Its stop commits from the turn's last checkpoint.
            if let Some(request) = row.cancel.clone()
                && turn_cancel::stops_before_resuming(&request, &row.phase)
            {
                let stopped = turn_cancel::restore(cx, self.services.as_ref(), &row, heads).await?;
                turn_cancel::finalize(cx, self.services.as_ref(), &row, &request, stopped, heads)
                    .await?;
                return await_stop(cx, self.services.as_ref(), session, &row.run).await;
            }
            // The turn's work on this owner runs under its span: a host's
            // `tracing` instrumentation of a provider, a tool or a plugin it
            // calls nests under the turn it serves (FIG-5353).
            let span = tracing::info_span!("lash.turn", session_id = %session, run_id = %row.run);
            let opened = match row.phase {
                UnfinishedPhase::Admitted => match heads.head(cx, session).await {
                    Ok(head) => lash_core_ids::operational_metrics::turn::phase(
                        "prompt/environment build",
                        None,
                        self.services.start(cx, &row, head).instrument(span.clone()),
                    )
                    .await
                    .map(|drive| OpenTurn {
                        drive,
                        pending: None,
                        row: row.clone(),
                    }),
                    Err(error) => Err(error),
                },
                UnfinishedPhase::Model { .. } | UnfinishedPhase::Tools { .. } => {
                    self.services
                        .resume(cx, TurnRestore::new(cx, &row, heads))
                        .instrument(span.clone())
                        .await
                }
            };
            let turn = match opened {
                Ok(turn) => turn,
                // A turn its committed state cannot prepare, on any pass (a
                // plugin build its recorded config refuses, FIG-5246, or a head
                // its creation never recorded, FIG-4553), ends with that refusal
                // as its run's cause: no pass would clear it.
                Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => {
                    phases::refuse(cx, self.services.tracing(), &row, refusal).await?;
                    return Ok(Pass::Again);
                }
                // The step an after-step request lets finish cannot run on a
                // build that does not decode its state: the request ends the
                // turn without it, as a cancel's restore does, so the refusal
                // never holds the session's one way out (FIG-5601).
                Err(error) if row.cancel.is_some() && resume_refusal(&error).is_some() => {
                    if let Some(request) = &row.cancel {
                        turn_cancel::finalize(
                            cx,
                            self.services.as_ref(),
                            &row,
                            request,
                            None,
                            heads,
                        )
                        .await?;
                    }
                    return await_stop(cx, self.services.as_ref(), session, &row.run).await;
                }
                Err(error) => return Err(error),
            };
            let ran = match Box::pin(
                phases::run_phases(cx, self.services.as_ref(), turn, heads).instrument(span),
            )
            .await
            {
                // A refusal no pass would clear, met as the turn finishes (an
                // after-turn callback's), ends the run with it, as one met at
                // preparation does: retrying the pass would meet it again.
                Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => {
                    phases::refuse(cx, self.services.tracing(), &row, refusal).await?;
                    return Ok(Pass::Again);
                }
                ran => ran?,
            };
            match ran {
                PhaseExit::Committed(_) | PhaseExit::CancelRequested => Ok(Pass::Again),
                PhaseExit::Cancelled => {
                    await_stop(cx, self.services.as_ref(), session, &row.run).await
                }
                PhaseExit::Lost => Ok(Pass::Lost),
                PhaseExit::Released => Ok(Pass::Released),
                PhaseExit::Drained => drain_release(cx).await,
                PhaseExit::Suspended { due } => {
                    let next_due = due.into_iter().chain(cx.next_due()).min();
                    let mut tx = cx.begin().await?;
                    tx.give_up(Release::Waiting { next_due });
                    cx.commit(tx, CommitLabel::SESSION_RELEASE).await?;
                    Ok(Pass::Released)
                }
            }
        })
        .await
    }
}
