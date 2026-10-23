//! Derive the next engine event from current control and active outcomes.
use super::*;

/// What the next event is.
#[expect(
    clippy::large_enum_variant,
    reason = "one per pass, moved straight into the engine call"
)]
pub(super) enum Next {
    Event(EngineEvent),
    /// A due wait was settled on the transaction: commit it and look again,
    /// since a resolution committed first wins.
    Committed,
    /// Nothing happened; the earliest due time.
    Nothing {
        due: Option<DurableInstant>,
    },
    /// A granted tool's call's resolution committed its match: lash admits
    /// the call of the matched tool in its place, without the engine.
    Matched(Box<Handoff>),
}

impl ProcessActivation {
    #[expect(
        clippy::too_many_arguments,
        reason = "the rows one event is derived from"
    )]
    pub(super) async fn next_event(
        &self,
        tx: &mut ActorTx,
        engine: &dyn ProcessEngine,
        process: &ProcessId,
        reads: &dyn DurableReads,
        state_rev: u64,
        driver: &mut Driver,
        fold: &RunFold,
        cancel: Option<(CancelOrigin, u64)>,
        payload: &serde_json::Value,
        now: DurableInstant,
    ) -> Result<Next, DurableError> {
        if let Some((origin, requested_at_ms)) = cancel
            && driver.grace_until.is_none()
        {
            // The grace runs from the committed request, so a transition
            // recomputed after a failed commit sees the same event.
            let grace_until = DurableInstant(i64::try_from(requested_at_ms).unwrap_or(i64::MAX))
                .after_millis(i64::try_from(driver.cancel_grace_ms).unwrap_or(i64::MAX));
            driver.grace_until = Some(grace_until.0);
            if let Some(seq) = tx
                .mail()
                .iter()
                .filter(|mail| mail.kind.as_str() == CANCEL_MAIL)
                .map(|mail| mail.seq)
                .max()
            {
                tx.ack_through(seq);
            }
            return Ok(Next::Event(EngineEvent::Cancelled {
                origin,
                grace_until,
            }));
        }
        if state_rev == 0 {
            return Ok(Next::Event(EngineEvent::Started {
                payload: payload.clone(),
            }));
        }
        if let Some(immediate) = driver.immediate.take() {
            return Ok(Next::Event(match immediate {
                Immediate::KeyPinned { name } => {
                    let pinned = driver.keys.get(&name).ok_or_else(|| {
                        corrupt("a process's pinned key", "the driver names no such key")
                    })?;
                    EngineEvent::KeyPinned {
                        name,
                        key: lash_execution_contracts::waits::PinnedKey::new(pinned.key.clone()),
                    }
                }
            }));
        }
        if let Some((step, outcome)) = settled_step(driver, fold) {
            // A granted tool's call's committed resolution is consumed by
            // lash (P6): a resolved match hands off to its target, and a
            // refused or unavailable one answers the engine, typed, from
            // the recorded decision.
            let matched = step
                .resolves
                .then(|| crate::runtime::process::steps::resolved_match(&outcome))
                .flatten();
            if let Some(matched) = matched {
                if let Some(decision) =
                    crate::runtime::process::steps::dispatch_decision(process, &matched)
                {
                    record_effect(tx, process, driver, &step, &decision, self.fleet())?;
                    return Ok(Next::Event(EngineEvent::StepSettled {
                        call_id: Some(step.call.clone()),
                        step: step.request.step().clone(),
                        outcome: decision,
                    }));
                }
                if let Some(route) = matched.resolved() {
                    let request = step.request.clone();
                    let StepRequest::Tool { input, .. } = &request else {
                        return Err(corrupt("dispatch", "resolution is not a tool use"));
                    };
                    let pin = crate::runtime::process::steps::InvocationTarget {
                        route: route.clone(),
                        contract: matched
                            .definition()
                            .ok_or_else(|| corrupt("dispatch", "missing definition"))?
                            .contract_id(),
                        args: input.clone(),
                        dispatch_requested: Some(matched.requested.clone()),
                    };
                    return Ok(Next::Matched(Box::new(Handoff {
                        resolution: step,
                        target: request,
                        pin,
                    })));
                }
                return Err(corrupt(
                    "a granted tool's resolution",
                    "its match retains no definition",
                ));
            }
            let outcome = self.steps.engine_output(process, &step.request, outcome);
            record_effect(tx, process, driver, &step, &outcome, self.fleet())?;
            if let StepRequest::Engine {
                step: name, kind, ..
            } = &step.request
            {
                record_ran(
                    tx,
                    process,
                    name,
                    engine.settled_ran(kind, &outcome),
                    self.fleet(),
                );
            }
            return Ok(Next::Event(EngineEvent::StepSettled {
                call_id: match &step.request {
                    StepRequest::Tool { .. } => Some(step.call.clone()),
                    StepRequest::Engine { .. } => None,
                },
                step: step.request.step().clone(),
                outcome,
            }));
        }
        let mut due = driver
            .grace_until
            .map(DurableInstant)
            .into_iter()
            .collect::<Vec<_>>();
        match driver.blocked.clone() {
            None | Some(Blocked::Idle) => {}
            Some(Blocked::Sleep { until, .. }) => {
                if now.0 >= until {
                    driver.blocked = None;
                    return Ok(Next::Event(EngineEvent::Woke));
                }
                due.push(DurableInstant(until));
            }
            Some(Blocked::External { name, wait, .. }) => {
                let row = reads
                    .wait(&wait.0)
                    .await?
                    .ok_or_else(|| corrupt("a pinned key's wait", "its row is gone"))?;
                match row.lifecycle.state() {
                    WaitState::Resolved => {
                        driver.blocked = None;
                        return Ok(Next::Event(EngineEvent::ExternalResolved {
                            name,
                            resolution: resolution(&row)?,
                        }));
                    }
                    WaitState::TimedOut | WaitState::Revoked => {
                        driver.blocked = None;
                        return Ok(Next::Event(EngineEvent::ExternalTimedOut { name }));
                    }
                    WaitState::Pending => match row.purpose.deadline() {
                        Some(deadline) if now >= deadline => {
                            tx.write(DomainWrite::Wait(WaitWrite::Due { id: wait.0 }));
                            return Ok(Next::Committed);
                        }
                        Some(deadline) => due.push(deadline),
                        None => {}
                    },
                }
            }
            Some(Blocked::Process {
                process: target,
                wait,
                ..
            }) => {
                // The wait row's committed winner is the answer, never the
                // target's live registry row: a timeout that committed first
                // stays a timeout, and a resolution outlives the target's
                // pruning.
                let mut row = process_wait(reads, &wait).await?;
                if row.lifecycle.state() == WaitState::Pending {
                    // A target that ended before the wait was pinned
                    // resolved no wait: resolve it from the target's
                    // recorded end, first winner, and read the winner.
                    waits::resolve_ended_terminal(
                        &self.backend,
                        &lash_execution_contracts::waits::WaitRef::new(
                            wait.0,
                            WaitKind::ProcessTerminal,
                        ),
                    )
                    .await?;
                    row = process_wait(reads, &wait).await?;
                }
                match row.lifecycle.state() {
                    WaitState::Resolved => {
                        driver.blocked = None;
                        return Ok(Next::Event(EngineEvent::ProcessEnded {
                            process: target,
                            outcome: waits::process_outcome(resolution(&row)?)?,
                        }));
                    }
                    WaitState::TimedOut | WaitState::Revoked => {
                        driver.blocked = None;
                        return Ok(Next::Event(EngineEvent::ProcessWaitTimedOut {
                            process: target,
                        }));
                    }
                    WaitState::Pending => match row.purpose.deadline() {
                        Some(deadline) if now >= deadline => {
                            tx.write(DomainWrite::Wait(WaitWrite::Due { id: wait.0 }));
                            return Ok(Next::Committed);
                        }
                        Some(deadline) => due.push(deadline),
                        None => {}
                    },
                }
            }
        }
        Ok(Next::Nothing {
            due: due.into_iter().min(),
        })
    }
}
