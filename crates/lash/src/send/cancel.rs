//! Cancel, withdraw (FIG-3600 S5b, D1 §1.6): an input still queued is
//! withdrawn; an input whose run executes has the run cancelled cooperatively
//! through its cancellation gate (ADR 0039). A plugin task's operation run is
//! withdrawn while no run entered it, and otherwise cancelled at its running
//! handler (FIG-5343).

use lash_core::facade_support::{
    OperationCancelOutcome, QueueWithdrawalObservation, TurnAddress, TurnCancelMode,
    TurnCancelOutcome, TurnCancelRequest, TurnCancelUndeliveredInputPolicy, TurnWorkDriver,
};
use lash_vocabulary::identity::RunIdCoreSupport;
use lash_vocabulary::identity::TurnId;

use super::follow::Subject;
use super::{CancelReceipt, SendParts};
use crate::error::{EmbedError, Result};

/// The request fields a cancel carries.
#[derive(Clone, Debug, Default)]
pub(crate) struct CancelRequestSpec {
    pub(crate) request_id: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) mode: TurnCancelMode,
    pub(crate) undelivered: TurnCancelUndeliveredInputPolicy,
}

pub(super) async fn apply(
    parts: &SendParts,
    subject: &Subject,
    request: CancelRequestSpec,
) -> Result<CancelReceipt> {
    let run = match subject {
        Subject::Run(run) => run.clone(),
        Subject::Input(input) => {
            // A cancelled input can retain its admitted run's binding.
            // Address that run even after it stopped: its terminal answers
            // UnknownOrRevoked, never a queued withdrawal.
            if let Some(run) = parts.store.run_of_input(&input.input_id).await? {
                run
            } else {
                // A cold attach by input id lacks the source key in its
                // synthetic receipt. Read the accepted row's naming data.
                let row = parts.store.pending_turn_input(&input.input_id).await?;
                lash_core::durable_port::domain::queued_input_run(
                    &input.input_id,
                    row.as_ref()
                        .and_then(|row| row.input.source_key.as_deref())
                        .or(input.source_key.as_deref()),
                )
                .ok_or_else(|| {
                    EmbedError::from(crate::SendError::Unresolved {
                        input_id: input.input_id.clone(),
                    })
                })?
            }
        }
    };
    let receipt = cancel_run(parts, &run, request.clone()).await?;
    // A checkpoint or composing admission may bind the input to another
    // run after the point read above. Resolve that binding once more when
    // its original queued address found nothing; both requests use the same
    // atomic cancel path, and only the consuming run can accept the second.
    if matches!(receipt, CancelReceipt::UnknownOrRevoked)
        && let Subject::Input(input) = subject
        && let Some(bound) = parts.store.run_of_input(&input.input_id).await?
        && bound != run
    {
        return cancel_run(parts, &bound, request).await;
    }
    Ok(receipt)
}

async fn cancel_run(
    parts: &SendParts,
    run: &TurnId,
    request: CancelRequestSpec,
) -> Result<CancelReceipt> {
    let request_id = request
        .request_id
        .clone()
        .unwrap_or_else(|| format!("cancel:run:{run}"));
    let backend = parts.effect_host.backend().clone();
    if lash_core::tool_run::OperationRun::for_run_id(parts.session_id.clone(), run).is_some() {
        // A plugin task's run: withdrawn while no run entered it, and
        // otherwise cancelled at its running handler (FIG-5343).
        let mut cancel = TurnCancelRequest::new(
            TurnAddress::new(parts.session_id.clone(), run.clone()),
            request_id,
            request.origin,
        )
        .undelivered(request.undelivered)
        .mode(request.mode);
        cancel.reason = request.reason;
        let outcome = TurnWorkDriver::new(backend)
            .request_operation_cancel(cancel)
            .await
            .map_err(EmbedError::Runtime)?;
        return Ok(match outcome {
            OperationCancelOutcome::Withdrawn => CancelReceipt::Withdrawn {
                run: lash_vocabulary::identity::RunId::from_turn_id(run.clone()),
                input: None,
            },
            OperationCancelOutcome::Cancelled(receipt) => CancelReceipt::Cancelled {
                run: lash_vocabulary::identity::RunId::from_turn_id(run.clone()),
                receipt: Box::new(receipt),
            },
            OperationCancelOutcome::UnknownOrRevoked => CancelReceipt::UnknownOrRevoked,
        });
    }
    let withdrawals = QueueWithdrawalObservation::new(
        backend.session_store_factory(),
        std::sync::Arc::clone(&parts.live_replay_store),
    );
    let driver = TurnWorkDriver::new(backend)
        .with_terminal_pacing(parts.observer_pacing.terminal)
        .publishing_withdrawals(std::sync::Arc::new(withdrawals));
    // The cancel addresses the run's running physical turn.
    let turn = driver
        .running_turn(&TurnAddress::new(parts.session_id.clone(), run.clone()))
        .await
        .map_err(EmbedError::Runtime)?;
    let mut cancel = TurnCancelRequest::new(turn, request_id, request.origin)
        .undelivered(request.undelivered)
        .mode(request.mode);
    cancel.reason = request.reason;
    let receipt = driver
        .request_cancel(cancel)
        .await
        .map_err(EmbedError::Runtime)?;
    Ok(match receipt.outcome {
        TurnCancelOutcome::Withdrawn { input } => CancelReceipt::Withdrawn {
            run: lash_vocabulary::identity::RunId::from_turn_id(run.clone()),
            input: Some(input),
        },
        TurnCancelOutcome::UnknownOrRevoked | TurnCancelOutcome::CompletionWonRace => {
            CancelReceipt::UnknownOrRevoked
        }
        TurnCancelOutcome::Requested(_)
        | TurnCancelOutcome::AlreadyRequested(_)
        | TurnCancelOutcome::Escalated(_)
        | TurnCancelOutcome::PolicyConflict { .. } => CancelReceipt::Cancelled {
            run: lash_vocabulary::identity::RunId::from_turn_id(run.clone()),
            receipt: Box::new(receipt),
        },
    })
}
