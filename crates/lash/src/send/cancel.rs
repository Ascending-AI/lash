//! Cancel, withdraw (FIG-3600 S5b, D1 §1.6): an input still queued is
//! withdrawn; an input whose run executes has the run cancelled cooperatively
//! through its cancellation gate (ADR 0039).

use lash_core::facade_support::{
    TurnAddress, TurnCancelMode, TurnCancelRequest, TurnCancelUndeliveredInputPolicy,
    TurnWorkDriver,
};
use lash_core::runtime::{
    PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget,
};
use lash_core::store::PhysicalTurn;
use lash_core::{InputId, TurnId};

use super::resolve::{self, Resolution};
use super::{CancelReceipt, CancelTarget, SendParts};
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
    target: &CancelTarget,
    request: CancelRequestSpec,
) -> Result<CancelReceipt> {
    match target {
        CancelTarget::Input(input) => cancel_input(parts, input, request).await,
        CancelTarget::Run(run) => {
            let request_id = request
                .request_id
                .clone()
                .unwrap_or_else(|| format!("cancel:run:{run}"));
            cancel_run(parts, run, request_id, request).await
        }
    }
}

async fn cancel_input(
    parts: &SendParts,
    input: &InputId,
    request: CancelRequestSpec,
) -> Result<CancelReceipt> {
    let outcome = parts
        .ops
        .cancel_pending_turn_input(&parts.store, input.as_str())
        .await?;
    let request_id = request
        .request_id
        .clone()
        .unwrap_or_else(|| format!("cancel:input:{input}"));
    match outcome {
        outcome @ (PendingTurnInputCancelOutcome::Cancelled(_)
        | PendingTurnInputCancelOutcome::AlreadyCancelled(_)) => Ok(CancelReceipt::Withdrawn(
            Box::new(PendingTurnInputCancelReceipt {
                target: PendingTurnInputCancelTarget::input_id(input.to_string()),
                outcome,
            }),
        )),
        PendingTurnInputCancelOutcome::NotFound => Ok(CancelReceipt::NotFound),
        PendingTurnInputCancelOutcome::AlreadyAdmitted { run, .. } => {
            cancel_run(parts, &run, request_id, request).await
        }
        // A completed input was applied by a committed turn, which is not
        // the run's end: a run that switched frames runs on in its
        // follow-on turns, so its run answers whether it settled.
        PendingTurnInputCancelOutcome::AlreadyCompleted(_) => {
            let run = run_of_input(parts, input).await?;
            cancel_run(parts, &run, request_id, request).await
        }
    }
}

/// The run that took `input`, from its durable binding: the admission binds
/// it before any turn commits, and the commit that applied a checkpoint
/// delivery binds it, so a cancel reaches the consuming run by one point
/// read.
async fn run_of_input(parts: &SendParts, input: &InputId) -> Result<TurnId> {
    parts.store.run_of_input(input).await?.ok_or_else(|| {
        EmbedError::from(crate::SendError::Unresolved {
            input_id: input.clone(),
        })
    })
}

async fn cancel_run(
    parts: &SendParts,
    run: &TurnId,
    request_id: String,
    request: CancelRequestSpec,
) -> Result<CancelReceipt> {
    if matches!(
        resolve::resolve_run(parts, run).await?,
        Resolution::Settled { .. } | Resolution::OperationSettled { .. }
    ) {
        return Ok(CancelReceipt::AlreadySettled { run: run.clone() });
    }
    if let Some(operation) =
        lash_core::tool_run::OperationRun::for_run_id(parts.session_id.clone(), run)
    {
        let batch = parts.store.list_queued_work().await?.into_iter().find(|batch| {
            batch.batch_id.as_str() == operation.operation_id
                && matches!(&batch.payload, crate::persistence::QueuedWorkPayload::SessionCommand { command }
                    if matches!(command.as_ref(), lash_core::facade_support::SessionCommand::RunPluginTask { .. }))
        });
        let Some(batch) = batch else {
            return Ok(
                if matches!(
                    resolve::resolve_run(parts, run).await?,
                    Resolution::OperationSettled { .. }
                ) {
                    CancelReceipt::AlreadySettled { run: run.clone() }
                } else {
                    CancelReceipt::NotFound
                },
            );
        };
        if parts
            .ops
            .cancel_queued_work_batch(&parts.store, &operation.operation_id)
            .await?
            .is_some()
        {
            return Ok(CancelReceipt::OperationWithdrawn { run: run.clone() });
        }
        let receipt = lash_core::runtime::SessionCommandReceipt {
            session_id: parts.session_id.clone(),
            batch_id: batch.batch_id,
            source_key: batch.source_key.unwrap_or_default(),
        };
        let request = lash_core::runtime::request_plugin_task_cancel(
            parts.store.store().as_ref(),
            &parts.effect_host,
            &receipt,
        )
        .await?;
        return Ok(CancelReceipt::OperationRequested {
            run: run.clone(),
            request,
        });
    }
    // The cancel addresses the run's running physical turn: the first of its
    // turns that has not committed.
    let mut ordinal = 0_u64;
    let turn = loop {
        let turn = PhysicalTurn::derive_turn_id(run, ordinal);
        let committed = parts
            .store
            .turn_is_committed(&TurnAddress::new(parts.session_id.clone(), turn.clone()))
            .await
            .unwrap_or(false);
        if !committed {
            break turn;
        }
        ordinal = ordinal.saturating_add(1);
    };
    let mut cancel = TurnCancelRequest::new(
        TurnAddress::new(parts.session_id.clone(), turn),
        request_id,
        request.origin,
    )
    .undelivered(request.undelivered)
    .mode(request.mode);
    cancel.reason = request.reason;
    let receipt = TurnWorkDriver::for_session(
        std::sync::Arc::clone(&parts.effect_host),
        parts.session_id.to_string(),
        std::sync::Arc::clone(parts.store.store()),
    )
    .request_cancel(cancel)
    .await
    .map_err(EmbedError::Runtime)?;
    Ok(CancelReceipt::Requested {
        run: run.clone(),
        receipt: Box::new(receipt),
    })
}
