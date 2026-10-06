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
//! it folds the cell's run records once, records `Interrupted` for a started
//! `Once` without an outcome, and answers the parked operation with the
//! folded outcome by its admitted identity. No earlier host operation runs
//! again, and no code runs against a recorded outcome: the only stretch
//! computed again is the effect-free one after the last snapshot.
//!
//! When the cell ends, its result commits as the execution's last snapshot
//! (`cell.snapshot`), so a turn restored later reads the result instead of
//! running the cell.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, BodyOutput, ExecutionDraft, PolicyView, Recovery, ToolBody,
};
use lash_core_store::tool_run::AttemptOutcome;
use lash_durable::domain::ExecKey;
use lash_durable::{CommitLabel, DomainWrite, DurableError};
use lash_sansio::ToolCallId;
use lash_vm_protocol::{FrameEpoch, OpaqueVmState, VmOwner, VmStateKind};
use lashlang::{
    AbilityOp, AbilityOutcome, CompiledProgram, ExecutionBound, ExecutionBounds,
    ExecutionHostError, ExecutionMode, ExecutionOutcome, ResourceOperation, Value,
    VmExecutionStart, VmInstance, VmRequest, VmResume, VmRunConfig, VmStep,
};

use crate::identity::CodeCallIdentities;
use crate::ledger::Checkpoint;
use crate::snapshot::{
    BrokerLedger, DurableSnapshotStore, IssuedOperation, OperationId, QuietPoint, StoredSnapshot,
    outcomes_to_inject,
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
    fn encode(&self) -> serde_json::Value {
        match self {
            Self::Finished(value) => serde_json::json!({ "finished": value }),
            Self::Failed(error) => serde_json::json!({ "failed": error }),
        }
    }

    fn decode(stored: &serde_json::Value) -> Option<Self> {
        if let Some(value) = stored.get("finished") {
            return Some(Self::Finished(value.clone()));
        }
        stored
            .get("failed")
            .and_then(serde_json::Value::as_str)
            .map(|error| Self::Failed(error.to_owned()))
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
    let store = DurableSnapshotStore::new(cx, cell.exec.clone());
    let config = VmRunConfig::new(
        ExecutionMode::Foreground,
        ExecutionBounds::new(ExecutionBound::Unbounded, ExecutionBound::Unbounded),
    );
    let executable = cell.program.executable_identity().as_str().to_owned();
    let mut instance = VmInstance::pristine();
    // Outcomes the parked operation is answered with, by admitted identity.
    let mut settled: BTreeMap<OperationId, BodyOutput> = BTreeMap::new();
    // The operation the VM stands on when it resumes from a continuation.
    let mut parked: Option<OperationId> = None;
    let mut resumed = false;
    let (mut broker, mut step) = match store.stored().await? {
        Some((_, StoredSnapshot::Ended { result, .. })) => {
            return CellEnd::decode(&result)
                .ok_or_else(|| CellError::Vm("the stored end does not decode".to_owned()));
        }
        Some((_, StoredSnapshot::Parked { checkpoint, broker })) => {
            resumed = true;
            recover(cx, &cell, &broker, &mut settled).await?;
            parked = broker.operations.keys().next_back().copied();
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
            (broker, step)
        }
        None => {
            cx.probe().vm_program_entered(&cell.exec);
            let broker = BrokerLedger {
                operations: BTreeMap::new(),
                next_admission: 0,
                frame_epoch: FrameEpoch(0),
            };
            let step = instance
                .start(
                    Arc::clone(&cell.program),
                    VmExecutionStart::Session,
                    config.clone(),
                )
                .map_err(vm_error)?;
            (broker, step)
        }
    };
    // The operation issued since the last quiet point, waiting for its park.
    let mut issued: Option<(OperationId, ResolvedOperation)> = None;
    let end = loop {
        step = match step {
            VmStep::Suspended(suspended) => {
                let resume = match suspended.request {
                    VmRequest::Effect(AbilityOp::ResourceOperation(operation)) => {
                        match parked.take() {
                            Some(operation_id) => answer(&cell, &settled, operation_id),
                            None => {
                                let operation_id = OperationId {
                                    run: broker.next_admission,
                                    ordinal: round::member_ordinal(0).0,
                                };
                                let call = cell.identities.call_id(operation_id.run);
                                match cell.operations.resolve(call, &operation) {
                                    Ok(resolved) => {
                                        issued = Some((operation_id, resolved));
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
                let Some((operation_id, resolved)) = issued.take() else {
                    return Err(CellError::Vm(
                        "the VM parked on no operation it issued".to_owned(),
                    ));
                };
                let bytes = parked_vm.continuation.to_bytes().map_err(vm_error)?;
                broker
                    .operations
                    .insert(operation_id, resolved.draft.call().clone());
                broker.next_admission += 1;
                let checkpoint = Checkpoint {
                    vm: OpaqueVmState::seal(
                        VmStateKind::Continuation,
                        VmOwner::new(cell.exec.stored()),
                        lashlang::vm_contract_versions(),
                        bytes.clone(),
                    ),
                    ledger: crate::ledger::LedgerSnapshot::default(),
                    frame_epoch: broker.frame_epoch,
                };
                let (_, admitted) = store
                    .commit_admitting(QuietPoint {
                        checkpoint,
                        broker: broker.clone(),
                        executable_identity: executable.clone(),
                        issued: vec![IssuedOperation {
                            operation: operation_id,
                            draft: resolved.draft,
                        }],
                        waits: Vec::new(),
                        with: std::mem::take(&mut with),
                    })
                    .await?;
                let output = execute(cx, &admitted, resolved.body).await?;
                settled.insert(operation_id, output);
                parked = Some(operation_id);
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
    if resumed || !broker.operations.is_empty() {
        store
            .commit_end(end.encode(), broker, executable, with)
            .await?;
    }
    Ok(end)
}

/// Fold the cell's records once: record `Interrupted` for every started
/// `Once` without an outcome, in one `round.outcome` transaction, and keep
/// each admitted operation's outcome to answer it with.
async fn recover(
    cx: &ActorContext,
    cell: &Cell<'_>,
    broker: &BrokerLedger,
    settled: &mut BTreeMap<OperationId, BodyOutput>,
) -> Result<(), CellError> {
    let rows = cx.durable_reads()?.run_records(&cell.exec.owner()).await?;
    let fold = round::fold(&rows, &cell.operations.policies())?;
    let interrupted: Vec<_> = fold
        .recoveries()
        .iter()
        .filter(|(_, recovery)| matches!(recovery, Recovery::Interrupt))
        .map(|(id, _)| id.clone())
        .collect();
    if !interrupted.is_empty() {
        let mut tx = cx.begin().await?;
        for id in &interrupted {
            round::settle_interrupted(&mut tx, &fold, id).map_err(vm_error)?;
        }
        cx.commit(tx, CommitLabel::ROUND_OUTCOME).await?;
    }
    for (operation, outcome) in outcomes_to_inject(broker, &fold) {
        let material = round::outcome_material(&outcome)
            .and_then(|material| fold.material(material))
            .map(str::to_owned);
        settled.insert(operation, BodyOutput { outcome, material });
    }
    Ok(())
}

/// Run an admitted operation's body and commit its outcome.
async fn execute(
    cx: &ActorContext,
    admitted: &[AdmittedExecution],
    body: ToolBody,
) -> Result<BodyOutput, CellError> {
    let [admitted] = admitted else {
        return Err(CellError::Vm(
            "a quiet point admits one operation".to_owned(),
        ));
    };
    let output = round::run_body(cx, admitted, body).await;
    let mut tx = cx.begin().await?;
    round::settle(&mut tx, admitted, output.clone(), None).map_err(vm_error)?;
    cx.commit(tx, CommitLabel::ROUND_OUTCOME).await?;
    Ok(output)
}

/// The answer to the operation a resumed VM stands on: its outcome, by the
/// identity it was admitted under.
fn answer(
    cell: &Cell<'_>,
    settled: &BTreeMap<OperationId, BodyOutput>,
    operation: OperationId,
) -> VmResume {
    match settled.get(&operation) {
        Some(output) => VmResume::Effect(
            cell.operations
                .value(&output.outcome, output.material.as_deref())
                .map(AbilityOutcome::Value),
        ),
        // Started and neither settled nor interrupted: a `Repeatable` that
        // runs again at its ordinal is L4's (FIG-5174) to drive.
        None => VmResume::Effect(Err(ExecutionHostError::new(
            "the parked operation has no outcome to resume with",
        ))),
    }
}
