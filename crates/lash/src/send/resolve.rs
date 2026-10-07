//! Outcome derivation (FIG-3600 S5b, D1 §1.5): an input's run, then that
//! run's terminal, read from the store alone.
//!
//! Resolution makes no engine call, so it answers the same after a restart. A
//! committed run answers from its terminal evidence, which the head commit
//! of its final physical turn writes with the outcome it committed: one
//! read, however deep the engine's queues are (FIG-4345).

use lash_core::facade_support::TurnOutcome;
use lash_core::runtime::TurnInputAcceptanceReceipt;
use lash_core::store::RunTerminalCause;
use lash_core::{InputId, TurnId};

use super::SendParts;
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
    NotAccepted,
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
    /// The input is open and its session carries a fault (ADR 0109 §9): no
    /// run admits it until an operator clears the fault, whose typed
    /// error is the answer.
    Faulted(lash_core::RuntimeError),
}

fn store_error(error: lash_core::StoreError) -> crate::EmbedError {
    crate::EmbedError::Store(error)
}

/// Resolve an accepted input through the durable binding made by its claim.
pub(super) async fn resolve_input(
    parts: &SendParts,
    receipt: &TurnInputAcceptanceReceipt,
) -> Result<Resolution> {
    if let Some(run) = parts
        .store
        .run_of_input(&receipt.input_id)
        .await
        .map_err(store_error)?
    {
        return resolve_run(parts, &run).await;
    }
    let open = parts
        .store
        .pending_turn_input(&receipt.input_id)
        .await
        .map_err(store_error)?
        .is_some();
    // Claim and settlement can race the pending read. Re-read the binding
    // before interpreting a missing row as a withdrawal.
    if let Some(run) = parts
        .store
        .run_of_input(&receipt.input_id)
        .await
        .map_err(store_error)?
    {
        return resolve_run(parts, &run).await;
    }
    if !open {
        return unbound_input(parts, &receipt.input_id).await;
    }
    match parts.store.session_fault().await {
        Ok(Some(fault)) => return Ok(Resolution::Faulted(fault.record.runtime_error())),
        Ok(None) | Err(lash_core::StoreError::UnsupportedStoreOperation { .. }) => {}
        Err(error) => return Err(store_error(error)),
    }
    Ok(Resolution::Undecided { run: None })
}

/// An input with no run binding and no open row: a withdrawal on record, or
/// no record at all. The input target's resolution reads the row in any
/// state: a terminal row no run is bound to is a withdrawal, and nothing
/// recorded is pending. A run that took the input since the reads above
/// answers it instead, on the next resolution.
async fn unbound_input(parts: &SendParts, input: &InputId) -> Result<Resolution> {
    let resolution = match parts
        .store
        .resolve_target(&lash_core::Target::Input(input.clone()))
        .await
    {
        Err(lash_core::StoreError::ForkTargetUnavailable { .. }) => Resolution::Withdrawn,
        Err(lash_core::StoreError::ForkTargetPending { .. }) => Resolution::NotAccepted,
        Ok(_) | Err(lash_core::StoreError::ForkTargetPruned { .. }) => {
            return Ok(Resolution::Undecided { run: None });
        }
        Err(error) => return Err(store_error(error)),
    };
    // An unavailable target is also a run that ended without a commit:
    // only an input no run is bound to is withdrawn.
    if parts
        .store
        .run_of_input(input)
        .await
        .map_err(store_error)?
        .is_some()
    {
        return Ok(Resolution::Undecided { run: None });
    }
    Ok(resolution)
}

/// Resolve a logical run from its durable record: its terminal evidence.
///
/// A run the head commit of its final physical turn ended is settled with
/// the outcome that commit wrote. A run whose execution
/// ended with a typed refusal is refused. Any other run is undecided.
pub(super) async fn resolve_run(parts: &SendParts, run: &TurnId) -> Result<Resolution> {
    let cause = parts
        .store
        .run_terminal(run)
        .await
        .map_err(store_error)?
        .map(|terminal| terminal.cause);
    match &cause {
        Some(RunTerminalCause::Committed { outcome, .. }) => {
            return Ok(Resolution::Settled {
                run: run.clone(),
                outcome: TurnOutcome::from(outcome.clone()),
            });
        }
        // A cancel the session actor honoured before the turn committed: the
        // run answers with its cancellation, and the head did not move.
        Some(RunTerminalCause::Cancelled { evidence }) => {
            return Ok(Resolution::Settled {
                run: run.clone(),
                outcome: TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled {
                    evidence: evidence.clone(),
                }),
            });
        }
        _ => {}
    }
    if let Some(operation) =
        lash_core::tool_run::OperationRun::for_run_id(parts.session_id.clone(), run)
    {
        let pending = parts
            .store
            .list_queued_work()
            .await
            .map_err(store_error)?
            .into_iter()
            .any(|batch| batch.batch_id.as_str() == operation.operation_id);
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
                lash_core::runtime::SessionCommandOutcome::Failed { code, message } => {
                    return Ok(Resolution::OperationSettled {
                        run: run.clone(),
                        outcome: Box::new(
                            lash_core::runtime::PluginOperationCommandOutcome::Refused {
                                error: Box::new(lash_core::RuntimeError::new(code, message)),
                            },
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
                | RunTerminalCause::SessionDeleted { .. }
                | RunTerminalCause::SubstrateLost {
                    cancelled_by: Some(_),
                },
            ) => Some(PluginOperationCommandOutcome::Cancelled),
            Some(RunTerminalCause::SubstrateLost { cancelled_by: None }) => {
                Some(PluginOperationCommandOutcome::Refused {
                    error: Box::new(lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::EngineRunSubstrateLost,
                        format!("the operation Run `{run}` lost its invocation"),
                    )),
                })
            }
            Some(RunTerminalCause::Refused {
                code,
                message,
                refusal_cause,
            }) => {
                let mut error = lash_core::RuntimeError::new(code.clone(), message.clone());
                error.cause = refusal_cause.clone();
                Some(PluginOperationCommandOutcome::Refused {
                    error: Box::new(error),
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
        Some(RunTerminalCause::Cancelled { evidence }) => {
            return Ok(Resolution::Settled {
                run: run.clone(),
                outcome: TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled {
                    evidence,
                }),
            });
        }
        Some(RunTerminalCause::Refused {
            code,
            message,
            refusal_cause,
        }) => {
            // The structured cause is the refusal's type: a session-retirement
            // refusal must answer as one, not as its bare code.
            let mut refusal = lash_core::RuntimeError::new(code, message);
            refusal.cause = refusal_cause;
            Some(refusal)
        }
        // An operator's end, the session's deletion or a lost run carries no
        // answer of its own, and a command run answers no send: its
        // commands settle through their own receipts.
        Some(
            RunTerminalCause::CommandsApplied
            | RunTerminalCause::OperatorCancelled { .. }
            | RunTerminalCause::Forked { .. }
            | RunTerminalCause::SessionDeleted { .. }
            | RunTerminalCause::SubstrateLost { .. },
        )
        | None => None,
    };
    if let Some(refusal) = refusal {
        return Ok(Resolution::Refused {
            run: run.clone(),
            refusal,
        });
    }
    Ok(Resolution::Undecided {
        run: Some(run.clone()),
    })
}
