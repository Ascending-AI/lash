//! Outcome derivation (FIG-3600 S5b, D1 §1.5): an input's run and that
//! run's terminal, read from the store alone.
//!
//! Resolution makes no engine call, so it answers the same after a restart. A
//! committed run answers from its terminal evidence, which the head commit
//! of its final physical turn writes with the outcome it committed: one
//! read, however deep the engine's queues are (FIG-4345), and the same read
//! answers the input's binding and the session's park (FIG-7045).

use lash_core::TurnId;
use lash_core::facade_support::TurnOutcome;
use lash_core::store::{
    FollowFacts, FollowSubject, InputFollowFacts, RunFollowFacts, RunTerminalCause,
};

use super::SendParts;
use super::follow::Subject;
use crate::error::Result;

/// What the store says about an input or a run, right now.
#[derive(Debug)]
pub(super) enum Resolution {
    /// Not settled yet. `run` is known once a turn applied the input.
    Undecided { run: Option<TurnId> },
    /// The input left the queue without any turn applying it, and its
    /// withdrawal is on record.
    Withdrawn,
    /// No record of the input exists: it was never accepted, or its
    /// withdrawal was reclaimed.
    Unknown,
    /// The host forsook the input's id before any input was filed under it.
    Forsaken,
    /// The run's final physical turn committed with `outcome`.
    Settled { run: TurnId, outcome: TurnOutcome },
    OperationSettled {
        run: TurnId,
        outcome: Box<lash_core::runtime::PluginOperationCommandOutcome>,
    },
    /// The run's execution ended with `refusal`, a typed refusal no retry could
    /// change, and no turn of it committed (FIG-4018).
    Refused {
        run: TurnId,
        refusal: lash_core::RuntimeError,
    },
    /// The run is open and its session parked (FIG-5367): it runs nothing
    /// until an operator redrives the session, and `reason` is why.
    Parked {
        run: TurnId,
        reason: crate::SessionParkReason,
    },
    /// The input is open and its session carries a fault (ADR 0109 §9): no
    /// run admits it until an operator clears the fault, whose typed
    /// error is the answer.
    Faulted(lash_core::RuntimeError),
}

fn store_error(error: lash_core::StoreError) -> crate::EmbedError {
    crate::EmbedError::Store(error)
}

/// A terminal handed over by its acknowledged writer, for this exact input
/// or run. The input ids are the commit's completed ids, never a previously
/// observed mutable run binding.
pub(super) fn committed(
    watch: &lash_durable::runner::KeyWatch,
    subject: &Subject,
) -> Option<Resolution> {
    let subject = match subject {
        Subject::Input(receipt) => FollowSubject::Input(receipt.input_id.clone()),
        Subject::Run(run) => FollowSubject::Run(run.clone()),
    };
    let (run, outcome) = watch.committed_turn(&subject)?;
    Some(Resolution::Settled {
        run,
        outcome: TurnOutcome::from(outcome),
    })
}

/// Resolve `subject` from one keyed snapshot of its facts (FIG-7045).
///
/// The snapshot answers an input's binding, whether its row is open, its
/// withdrawal and its session's fault together with the bound run's terminal
/// and its session's park: the claim that binds an input moves its row in
/// the same transaction, so a row no longer open with no binding beside it
/// is no claim racing the read (FIG-5915). Nothing is carried from one
/// resolution to the next: a fork or release can rebind the input, so each
/// resolution reads its binding afresh.
pub(super) async fn resolve(parts: &SendParts, subject: &Subject) -> Result<Resolution> {
    resolve_facts(
        parts,
        facts(
            parts,
            subject,
            None,
            lash_core::facade_support::FollowRead::First,
        )
        .await?,
    )
    .await
}

pub(super) async fn facts(
    parts: &SendParts,
    subject: &Subject,
    watch: Option<&mut lash_durable::runner::KeyWatch>,
    trigger: lash_core::facade_support::FollowRead,
) -> Result<FollowFacts> {
    if matches!(trigger, lash_core::facade_support::FollowRead::First) && watch.is_some() {
        // Form the cohort before claiming its read. Already runnable hint
        // delivery and sibling subscribers get one executor pass, so the
        // read consumes the newest hint instead of immediately rereading it.
        tokio::task::yield_now().await;
    }
    let subject = match subject {
        Subject::Input(receipt) => FollowSubject::Input(receipt.input_id.clone()),
        Subject::Run(run) => FollowSubject::Run(run.clone()),
    };
    match watch {
        Some(watch) if !matches!(trigger, lash_core::facade_support::FollowRead::Timer) => {
            watch
                .read_follow_facts(&parts.store, &subject, |heard| {
                    lash_core::facade_support::witness_follow_heard(heard);
                    lash_core::facade_support::witness_follow_read(trigger);
                })
                .await
        }
        watch => {
            // A fallback poll must start a fresh snapshot even if an old
            // read still has waiters: its hint could have been lost.
            if let Some(watch) = watch {
                lash_core::facade_support::witness_follow_heard(watch.seen());
            }
            lash_core::facade_support::witness_follow_read(trigger);
            parts.store.follow_facts(&subject).await
        }
    }
    .map_err(store_error)
}

pub(super) async fn resolve_facts(parts: &SendParts, facts: FollowFacts) -> Result<Resolution> {
    match facts {
        FollowFacts::Input(input) => resolve_input(parts, input).await,
        FollowFacts::Run(run) => resolve_run(parts, run).await,
    }
}

/// Resolve an accepted input through the durable binding made by its claim.
async fn resolve_input(parts: &SendParts, input: InputFollowFacts) -> Result<Resolution> {
    if let Some(run) = input.run {
        // An operation's run is followed by its id: its batch is the
        // operation's own fact, keyed by the run.
        if lash_core::tool_run::OperationRun::for_run_id(parts.session_id.clone(), &run.run)
            .is_some()
        {
            return Box::pin(resolve(parts, &Subject::Run(run.run))).await;
        }
        return resolve_run(parts, run).await;
    }
    if input.forsaken {
        return Ok(Resolution::Forsaken);
    }
    // An input no run is bound to whose row is settled: a withdrawal on
    // record, or no record at all.
    if !input.open {
        return Ok(if input.withdrawn {
            Resolution::Withdrawn
        } else {
            Resolution::Unknown
        });
    }
    if let Some(fault) = input.fault {
        return Ok(Resolution::Faulted(fault.record.runtime_error()));
    }
    Ok(Resolution::Undecided { run: None })
}

/// Resolve a logical run from its durable record: its terminal evidence.
///
/// A run the head commit of its final physical turn ended is settled with
/// the outcome that commit wrote. A run whose execution
/// ended with a typed refusal is refused. Any other run is parked while its
/// session's actor is parked, and undecided otherwise.
async fn resolve_run(parts: &SendParts, facts: RunFollowFacts) -> Result<Resolution> {
    let RunFollowFacts {
        run,
        terminal,
        operation_pending,
        park,
    } = facts;
    let cause = terminal.map(|terminal| terminal.cause);
    let run = &run;
    match &cause {
        Some(RunTerminalCause::Committed { outcome, .. }) => {
            return Ok(Resolution::Settled {
                run: run.clone(),
                outcome: TurnOutcome::from(outcome.clone()),
            });
        }
        // A cancel the session actor honoured: the run answers with its
        // cancellation whether or not its stop published a head.
        Some(RunTerminalCause::Cancelled { evidence, .. }) => {
            return Ok(Resolution::Settled {
                run: run.clone(),
                outcome: TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled {
                    evidence: evidence.clone(),
                }),
            });
        }
        _ => {}
    }
    if let Some(pending) = operation_pending
        && let Some(operation) =
            lash_core::tool_run::OperationRun::for_run_id(parts.session_id.clone(), run)
    {
        if let Some(completion) = parts
            .store
            .queued_work_batch_completion(&operation.operation_id)
            .await
            .map_err(store_error)?
            && let Some((_, outcome)) = completion
                .command_outcomes
                .into_iter()
                .find(|(batch, _)| batch.as_str() == operation.operation_id)
        {
            match outcome {
                lash_core::runtime::SessionCommandOutcome::PluginOperation { outcome } => {
                    return Ok(Resolution::OperationSettled {
                        run: run.clone(),
                        outcome: Box::new(outcome),
                    });
                }
                lash_core::runtime::SessionCommandOutcome::Failed { refusal } => {
                    return Ok(Resolution::OperationSettled {
                        run: run.clone(),
                        outcome: Box::new(
                            lash_core::runtime::PluginOperationCommandOutcome::Refused { refusal },
                        ),
                    });
                }
                _ => {}
            }
        }
        use lash_core::runtime::PluginOperationCommandOutcome;
        let ended = match &cause {
            Some(
                RunTerminalCause::Cancelled { .. }
                | RunTerminalCause::OperatorCancelled { .. }
                | RunTerminalCause::Forked { .. }
                | RunTerminalCause::SessionDeleted { .. },
            ) => Some(PluginOperationCommandOutcome::Cancelled),
            Some(RunTerminalCause::Refused { refusal }) => {
                Some(PluginOperationCommandOutcome::Refused {
                    refusal: refusal.clone(),
                })
            }
            _ => None,
        };
        if let Some(outcome) = ended {
            return Ok(Resolution::OperationSettled {
                run: run.clone(),
                outcome: Box::new(outcome),
            });
        }
        if !pending && cause.is_none() {
            return Ok(Resolution::Withdrawn);
        }
    }
    let refusal = match cause {
        Some(RunTerminalCause::Committed { outcome, .. }) => {
            return Ok(Resolution::Settled {
                run: run.clone(),
                outcome: TurnOutcome::from(outcome),
            });
        }
        Some(RunTerminalCause::Cancelled { evidence, .. }) => {
            return Ok(Resolution::Settled {
                run: run.clone(),
                outcome: TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled {
                    evidence,
                }),
            });
        }
        // The structured cause is the refusal's type: a session-retirement
        // refusal must answer as one, not as its bare code.
        Some(RunTerminalCause::Refused { refusal }) => Some(lash_core::RuntimeError::from(refusal)),
        // An operator's end or the session's deletion carries no
        // answer of its own, and a command run answers no send: its
        // commands settle through their own receipts.
        Some(
            RunTerminalCause::CommandsApplied
            | RunTerminalCause::OperatorCancelled { .. }
            | RunTerminalCause::Forked { .. }
            | RunTerminalCause::SessionDeleted { .. },
        )
        | None => None,
    };
    if let Some(refusal) = refusal {
        return Ok(Resolution::Refused {
            run: run.clone(),
            refusal,
        });
    }
    // A run with no end whose session parked is the session's open run: a
    // session runs one run at a time, and its park holds that run.
    if let Some(reason) = park
        .as_deref()
        .map(crate::durable_session::session_park_reason)
        .transpose()?
    {
        return Ok(Resolution::Parked {
            run: run.clone(),
            reason,
        });
    }
    Ok(Resolution::Undecided {
        run: Some(run.clone()),
    })
}
