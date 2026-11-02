//! Translate broker failures into cell results or typed attempt refusals.

use super::*;

/// How a broker failure answers the cell. Nothing about the guest: the
/// cell's last checkpoint stands.
pub(super) fn broker_failure(
    state: &mut CodeModeExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    failure: KernelFailure,
    runs_on: bool,
    collected: &CellHostLedgers,
    printed: &[lash_core::AttachmentRef],
    died: &cell_run::DiedRunning<'_>,
) -> ExecResponse {
    let message = failure.to_string();
    let host = |message: String| {
        exec_response_from(
            collected,
            printed,
            lash_core::CellOutcome::Failed(lash_core::CellFailure::new(
                lash_core::CellFailureKind::Host,
                message,
            )),
        )
    };
    match failure {
        KernelFailure::Durable(error) => {
            let fault = match error {
                lash_durable::DurableError::RevisionConflict {
                    actor,
                    epoch,
                    expected,
                    actual,
                } => lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::OwnerRevisionConflict,
                    message.clone(),
                )
                .with_cause(lash_core::RuntimeErrorCause::OwnerRevisionConflict(
                    Box::new(lash_core::OwnerRevisionConflict {
                        actor: actor.as_str().to_owned(),
                        epoch: epoch.0,
                        expected: expected.0,
                        actual: actual.0,
                    }),
                )),
                lash_durable::DurableError::OwnershipLost(_) => lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::SessionExecutionLeaseLost,
                    message.clone(),
                ),
                _ => lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::RuntimeStore,
                    message.clone(),
                ),
            };
            ctx.record_nested_effect_error(
                lash_core::RuntimeEffectControllerError::from(fault)
                    .retryable_uncommitted_derivation(),
            );
            if ctx.is_cancelled() {
                state.rollback_code_execution();
            }
            host(message)
        }
        KernelFailure::Worker { outcome } => {
            // A limit the run itself exhausted is the cell's result.
            if let lash_protocol_types::engine_fault::InfrastructureOutcome::WorkerLimitExceeded {
                limit,
            } = &outcome
                && !limit.is_host_verdict()
            {
                return exec_response_from(
                    collected,
                    printed,
                    lash_core::CellOutcome::Failed(crate::feedback::worker_limit_failure(*limit)),
                );
            }
            // A worker that died running the cell ends it once its death is
            // the cell's to read; until then the pass fails as the host's
            // verdict below, and the next runs the cell again (FIG-6806).
            if let lash_protocol_types::engine_fault::InfrastructureOutcome::RunCrashedWorker {
                evidence,
            } = &outcome
                && let Some(failure) = died.answer(*evidence, &collected.executed_calls())
            {
                return exec_response_from(
                    collected,
                    printed,
                    lash_core::CellOutcome::Failed(failure),
                );
            }
            // The state a resumed cell runs on from is refused: another
            // build wrote it. That is no failure the model sees: the cell
            // aborts with the typed refusal, and its turn parks on the
            // cell's checkpoint.
            if runs_on
                && let lash_protocol_types::engine_fault::InfrastructureOutcome::RunRefused {
                    refusal,
                } = &outcome
                && matches!(
                    refusal,
                    lash_protocol_types::engine_fault::RunRefusal::Restore { .. }
                )
            {
                let mut refused = lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::VmWorkerFailed,
                    message.clone(),
                );
                refused.cause = Some(lash_core::RuntimeErrorCause::CellSnapshotUndecodable {
                    refusal: Box::new(refusal.clone()),
                });
                ctx.record_nested_effect_error(
                    lash_core::RuntimeEffectControllerError::from(refused)
                        .retryable_uncommitted_derivation(),
                );
                return host(message);
            }
            fail_attempt_on_host_verdict(ctx, &lash_vm_client::PoolError::from(outcome));
            if ctx.is_cancelled() {
                state.rollback_code_execution();
            }
            host(message)
        }
        // The parked state is refused where the machine imports it: the
        // same account as a worker's refusal of it.
        KernelFailure::Open(_) | KernelFailure::Sealed(_) | KernelFailure::EmptyCheckpoint
            if runs_on =>
        {
            ctx.record_nested_effect_error(
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed,
                    message.clone(),
                )
                .retryable_uncommitted_derivation(),
            );
            host(message)
        }
        KernelFailure::Diverged(reason) => {
            ctx.record_nested_runtime_effect_error(
                cell_carry::Error::Journal(reason).into_runtime(),
            );
            host(message)
        }
        // The broker's own invariant failed over sound records: every pass
        // meets it again, so the turn ends with it, typed, never retried.
        KernelFailure::Invariant(_) => {
            ctx.record_nested_effect_error(lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::BrokerInvariant,
                message.clone(),
            ));
            if ctx.is_cancelled() {
                state.rollback_code_execution();
            }
            host(message)
        }
        // The parent's own store or host faulted: the attempt fails
        // retryably, and the cell resumes from its last checkpoint.
        KernelFailure::Checkpoint(_) | KernelFailure::Parent(_) => {
            ctx.record_nested_effect_error(
                lash_core::RuntimeEffectControllerError::retryable_response_derivation(
                    message.clone(),
                ),
            );
            if ctx.is_cancelled() {
                state.rollback_code_execution();
            }
            host(message)
        }
        _ => host(message),
    }
}
