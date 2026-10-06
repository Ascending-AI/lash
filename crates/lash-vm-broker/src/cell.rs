//! A code cell run from its snapshot (ADR 0132 §8; V0, FIG-5170, then L7).
//!
//! A cell runs on the one heap VM until it issues a host operation: a quiet
//! point. The runner parks the VM there and commits, in one
//! `cell.snapshot+admit` transaction, the VM state, the broker ledger that
//! names the operation by its admitted identity, the operation's admission and
//! `x_start`, and the owner's own rows that ride with it. Only then does the
//! operation's body run; its outcome commits as `round.outcome`. The VM then
//! resumes from the state it parked in, and the operation it stands on is
//! answered with that outcome.
//!
//! A runner that finds a snapshot resumes from it, never from instruction 0:
//! the store recovers the operation the VM stands on by its admitted
//! identity (its saved outcome; `Interrupted`, committed as `cell.inject`,
//! for a started `Once`; or its body again for a started `Repeatable`), and
//! the VM is answered with it when it re-issues the operation. No earlier
//! host operation runs again, and no code runs against a recorded outcome:
//! the only stretch computed again is the effect-free one after the last
//! snapshot.
//!
//! When the cell ends, its result commits as the execution's last snapshot
//! (`cell.snapshot`), so a turn restored later reads the result instead of
//! running the cell.
//!
//! This is the in-process driver of the quiet-point protocol the
//! [`Broker`](crate::Broker) drives for a worker: both commit through one
//! [`DurableSnapshotStore`], in one stored form.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::round::{
    self, BodyOutput, ExecutionDraft, PolicyView, ToolBody,
};
use lash_core_store::tool_run::AttemptOutcome;
use lash_durable::domain::ExecKey;
use lash_durable::{DomainWrite, DurableError};
use lash_sansio::ToolCallId;
use lash_vm_protocol::{
    EffectKind, EncodedPayload, FrameEpoch, OpaqueVmState, VmOwner, VmStateKind,
};
use lashlang::{
    AbilityOp, AbilityOutcome, CompiledProgram, ExecutionBound, ExecutionBounds,
    ExecutionHostError, ExecutionMode, ExecutionOutcome, ResourceOperation, Value,
    VmExecutionStart, VmInstance, VmRequest, VmResume, VmRunConfig, VmStep,
};

use crate::authority::{RequestFingerprint, ResolvedRequest};
use crate::identity::CodeCallIdentities;
use crate::ledger::{Checkpoint, RecordedEnd};
use crate::snapshot::{
    BrokerLedger, DurableSnapshotStore, OperationId, PendingOperation, QuietPoint, Recovered,
    SnapshotStore as _,
};

/// One host operation a cell issued, as its host resolved it: what to admit
/// and the body that runs once the admission commits.
pub struct ResolvedOperation {
    /// The execution to admit.
    pub draft: ExecutionDraft,
    /// Its body.
    pub body: ToolBody,
}

/// The host operations a cell may issue.
pub trait CellOperations: Send + Sync {
    /// Resolve `operation`, issued under `call`, into its admission and body.
    ///
    /// # Errors
    ///
    /// The refusal the VM receives as the operation's failure.
    fn resolve(
        &self,
        call: ToolCallId,
        operation: &ResourceOperation,
    ) -> Result<ResolvedOperation, ExecutionHostError>;

    /// The value the VM receives for a settled outcome and the payload of
    /// the material it names.
    ///
    /// # Errors
    ///
    /// The refusal the VM receives as the operation's failure: an
    /// `Interrupted`, failed or timed-out outcome among them.
    fn value(
        &self,
        outcome: &AttemptOutcome,
        material: Option<&str>,
    ) -> Result<Value, ExecutionHostError>;

    /// The policies the tools currently declare, for the fold's veto.
    fn policies(&self) -> PolicyView;
}

/// How a cell ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellEnd {
    /// It finished with this value, encoded as JSON.
    Finished(serde_json::Value),
    /// It failed in the guest with this error.
    Failed(String),
}

impl CellEnd {
    /// The end as its last snapshot records it: the value, or the error, as
    /// JSON.
    fn recorded(&self) -> Result<RecordedEnd, CellError> {
        Ok(match self {
            Self::Finished(value) => RecordedEnd::Complete {
                value: EncodedPayload(serde_json::to_vec(value).map_err(vm_error)?),
            },
            Self::Failed(error) => RecordedEnd::GuestError {
                error: EncodedPayload(error.clone().into_bytes()),
            },
        })
    }

    /// The end `recorded` holds.
    ///
    /// # Errors
    ///
    /// [`CellError::Vm`] when it does not decode.
    pub fn of(recorded: &RecordedEnd) -> Result<Self, CellError> {
        match recorded {
            RecordedEnd::Complete { value } => serde_json::from_slice(&value.0)
                .map(Self::Finished)
                .map_err(vm_error),
            RecordedEnd::GuestError { error } => String::from_utf8(error.0.clone())
                .map(Self::Failed)
                .map_err(vm_error),
        }
    }
}

/// Why a cell could not run to its end; nothing it did not commit happened.
#[derive(Debug, thiserror::Error)]
pub enum CellError {
    /// The store refused, ownership loss included.
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// A quiet point did not commit.
    #[error(transparent)]
    QuietPoint(#[from] crate::ledger::QuietPointRefusal),
    /// The stored rows did not fold.
    #[error(transparent)]
    Fold(#[from] round::FoldRefusal),
    /// The VM refused a step or a stored state.
    #[error("the cell's VM refused: {0}")]
    Vm(String),
}

/// One cell to run: its execution, its compiled program, the identities its
/// operations' calls take and the host operations it may issue.
pub struct Cell<'a> {
    /// The execution.
    pub exec: ExecKey,
    /// The compiled program.
    pub program: Arc<CompiledProgram>,
    /// The identities its calls take.
    pub identities: CodeCallIdentities,
    /// Its host operations.
    pub operations: &'a dyn CellOperations,
}

fn vm_error(error: impl std::fmt::Display) -> CellError {
    CellError::Vm(error.to_string())
}

fn host_value(value: Value) -> VmResume {
    VmResume::Effect(Ok(AbilityOutcome::Value(value)))
}

/// What the operation a resumed VM stands on is answered with when it
/// re-issues it.
enum Answer {
    /// Its outcome.
    Output(Box<BodyOutput>),
    /// Its body, run again under its identity.
    Rerun,
}

/// Run `cell` from its latest snapshot, or from the start when it has none,
/// to its end. `with` are the owner's rows that commit with the cell's first
/// commit (its first quiet point, or its end when it resumed); a cell that
/// issues no operation and had no snapshot commits nothing, and `with` is
/// dropped with it: its stretch is effect-free and is recomputed.
///
/// # Errors
///
/// [`CellError`]; ownership loss among them, after which the caller drops
/// everything it holds.
pub async fn run_cell(
    cx: &ActorContext,
    cell: Cell<'_>,
    mut with: Vec<DomainWrite>,
) -> Result<CellEnd, CellError> {
    let store =
        DurableSnapshotStore::new(cx, cell.exec.clone()).with_policies(cell.operations.policies());
    let config = VmRunConfig::new(
        ExecutionMode::Foreground,
        ExecutionBounds::new(ExecutionBound::Unbounded, ExecutionBound::Unbounded),
    );
    let owner = VmOwner::new(cell.exec.stored());
    let mut instance = VmInstance::pristine();
    let mut answer: Option<Answer> = None;
    // The cell's ledger keeps every operation it admitted: it grants no
    // handle that would let an earlier one go.
    let (mut ledger, mut parked_state, mut step) = match store.latest().await? {
        Some((_, checkpoint)) => {
            if let Some(end) = &checkpoint.end {
                return CellEnd::of(end);
            }
            if let Some(pending) = &checkpoint.ledger.pending {
                answer = Some(match store.recover_output(pending).await? {
                    Recovered::Settled(output) => Answer::Output(Box::new(output)),
                    Recovered::Interrupted => Answer::Output(Box::new(BodyOutput {
                        outcome: AttemptOutcome::Interrupted,
                        material: None,
                    })),
                    Recovered::Rerun { .. } => Answer::Rerun,
                });
            }
            let continuation = instance
                .open_continuation(checkpoint.vm.bytes())
                .map_err(vm_error)?;
            let step = instance
                .start(
                    Arc::clone(&cell.program),
                    VmExecutionStart::Continuation(Box::new(continuation)),
                    config.clone(),
                )
                .map_err(vm_error)?;
            (checkpoint.ledger, Some(checkpoint.vm), step)
        }
        None => {
            cx.probe().vm_program_entered(&cell.exec);
            let ledger = BrokerLedger {
                operations: BTreeMap::new(),
                next_admission: 0,
                frame_epoch: FrameEpoch(0),
                grants: BTreeMap::new(),
                pending: None,
            };
            let step = instance
                .start(
                    Arc::clone(&cell.program),
                    VmExecutionStart::Session,
                    config.clone(),
                )
                .map_err(vm_error)?;
            (ledger, None, step)
        }
    };
    // The operation issued since the last quiet point, waiting for its park.
    let mut issued: Option<ResolvedOperation> = None;
    let end = loop {
        step = match step {
            VmStep::Suspended(suspended) => {
                let resume = match suspended.request {
                    VmRequest::Effect(AbilityOp::ResourceOperation(operation)) => {
                        match answer.take() {
                            Some(Answer::Output(output)) => {
                                ledger.pending = None;
                                respond(&cell, &output)
                            }
                            Some(Answer::Rerun) => {
                                let (call, id) = standing(&cell, &ledger)?;
                                match cell.operations.resolve(call, &operation) {
                                    Ok(resolved) => {
                                        let output = execute(cx, &store, id, resolved.body).await?;
                                        ledger.pending = None;
                                        respond(&cell, &output)
                                    }
                                    Err(error) => VmResume::Effect(Err(error)),
                                }
                            }
                            None => {
                                let run = ledger.next_admission;
                                let call = cell.identities.call_id(run);
                                match cell.operations.resolve(call, &operation) {
                                    Ok(resolved) => {
                                        ledger.next_admission += 1;
                                        ledger.pending = Some(pending(run, &operation)?);
                                        issued = Some(resolved);
                                        VmResume::Park
                                    }
                                    Err(error) => VmResume::Effect(Err(error)),
                                }
                            }
                        }
                    }
                    VmRequest::Effect(AbilityOp::Finish(value)) => host_value(value),
                    VmRequest::Effect(AbilityOp::Print(_)) => {
                        VmResume::Effect(Ok(AbilityOutcome::Unit))
                    }
                    VmRequest::Effect(_) => VmResume::Effect(Err(ExecutionHostError::new(
                        "this ability is not available to a durable cell yet",
                    ))),
                    VmRequest::CancelCheckpoint(_) => VmResume::CancelCheckpoint {
                        cancelled: cx.cancel().is_cancelled(),
                    },
                    VmRequest::Boundary | VmRequest::ParkDeclined(_) => VmResume::Continue,
                };
                instance.resume(resume).map_err(vm_error)?
            }
            VmStep::Parked(parked_vm) => {
                let Some(resolved) = issued.take() else {
                    return Err(CellError::Vm(
                        "the VM parked on no operation it issued".to_owned(),
                    ));
                };
                let bytes = parked_vm.continuation.to_bytes().map_err(vm_error)?;
                let state = OpaqueVmState::seal(
                    VmStateKind::Continuation,
                    owner.clone(),
                    lashlang::vm_contract_versions(),
                    bytes.clone(),
                );
                let committed = store
                    .commit_quiet_point(QuietPoint {
                        checkpoint: Checkpoint {
                            vm: state.clone(),
                            ledger: ledger.clone(),
                            host: None,
                            end: None,
                        },
                        admit: Some(resolved.draft),
                        waits: Vec::new(),
                        with: std::mem::take(&mut with),
                    })
                    .await?;
                ledger = committed.checkpoint.ledger;
                parked_state = Some(state);
                let (_, id) = standing(&cell, &ledger)?;
                answer = Some(Answer::Output(Box::new(
                    execute(cx, &store, id, resolved.body).await?,
                )));
                let continuation = instance.open_continuation(&bytes).map_err(vm_error)?;
                instance
                    .start(
                        Arc::clone(&cell.program),
                        VmExecutionStart::Continuation(Box::new(continuation)),
                        config.clone(),
                    )
                    .map_err(vm_error)?
            }
            VmStep::Complete(complete) => {
                break match complete.outcome {
                    ExecutionOutcome::Finished(value) => {
                        CellEnd::Finished(serde_json::to_value(&value).map_err(vm_error)?)
                    }
                    ExecutionOutcome::Continued => CellEnd::Finished(serde_json::Value::Null),
                    ExecutionOutcome::Failed(value) => {
                        CellEnd::Failed(serde_json::to_value(&value).map_err(vm_error)?.to_string())
                    }
                };
            }
            VmStep::GuestError(error) => break CellEnd::Failed(error.failure.error.to_string()),
        };
    };
    // A cell that resumed or admitted anything has a snapshot: its end
    // replaces it.
    if let Some(state) = parked_state {
        store
            .commit_quiet_point(QuietPoint {
                checkpoint: Checkpoint {
                    vm: state,
                    ledger,
                    host: None,
                    end: Some(end.recorded()?),
                },
                admit: None,
                waits: Vec::new(),
                with,
            })
            .await?;
    }
    Ok(end)
}

/// The operation a cell's VM stands on: the call it settles and its
/// admitted identity.
fn standing(
    cell: &Cell<'_>,
    ledger: &BrokerLedger,
) -> Result<(ToolCallId, OperationId), CellError> {
    let pending = ledger
        .pending
        .as_ref()
        .ok_or_else(|| CellError::Vm("the VM stands on no operation".to_owned()))?;
    let id = pending
        .operation
        .ok_or_else(|| CellError::Vm("the VM's operation was never admitted".to_owned()))?;
    Ok((cell.identities.call_id(pending.run), id))
}

/// The ledger's entry for `operation`, issued as admission `run`.
fn pending(run: u64, operation: &ResourceOperation) -> Result<PendingOperation, CellError> {
    let request = EncodedPayload(rmp_serde::to_vec_named(operation).map_err(vm_error)?);
    Ok(PendingOperation {
        run,
        kind: EffectKind::ResourceOperation,
        fingerprint: RequestFingerprint::of(&ResolvedRequest::Control {
            kind: EffectKind::ResourceOperation,
            payload: request.clone(),
        }),
        request,
        operation: None,
        waits: Vec::new(),
    })
}

/// Run admitted `operation`'s body and commit its outcome.
async fn execute(
    cx: &ActorContext,
    store: &DurableSnapshotStore,
    operation: OperationId,
    body: ToolBody,
) -> Result<BodyOutput, CellError> {
    let admitted = store.admitted(operation).ok_or_else(|| {
        CellError::Vm(format!("operation {operation:?} has no admitted execution"))
    })?;
    let output = round::run_body(cx, &admitted, body).await;
    store.settle_output(operation, output.clone()).await?;
    Ok(output)
}

/// The VM's answer for an operation's outcome.
fn respond(cell: &Cell<'_>, output: &BodyOutput) -> VmResume {
    VmResume::Effect(
        cell.operations
            .value(&output.outcome, output.material.as_deref())
            .map(AbilityOutcome::Value),
    )
}
