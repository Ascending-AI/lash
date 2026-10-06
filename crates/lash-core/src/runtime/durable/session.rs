//! The session activation and the turn's phases (ADR 0132 §4). Owned by V0
//! (FIG-5170), then L3 (FIG-5172).
//!
//! # Contracts
//!
//! - The durable entry is: load rows, [`restore_turn`] (exactly one
//!   `restore_from_checkpoint`), re-deliver the pending effect, then
//!   [`run_phases`]. The turn driver becomes this phase runner; it commits at
//!   the catalog's labels (`turn.admit`, `turn.prepare`, `model.start`,
//!   `model.done`, `round.present+model.start`, `turn.commit`) and never
//!   re-executes orchestration to reach a recorded outcome.
//! - The activation calls `session_mail::drain_session_mail` first on every
//!   claim and acts on what it returns.
//! - Extension points that let L3, L4 and L6b work in parallel: the
//!   `model.done` transaction calls `round::admit_round` (S4); the
//!   `round.present+model.start` transaction calls `round::present`; the
//!   `turn.commit` transaction calls `process::end_scope(tx,
//!   ScopeKey::Turn(..), batch)` (S6). L3 owns the transactions; the callee's
//!   owner owns what it writes.
//! - The checkpoint is L3a's bounded encoding, referenced from
//!   [`TurnRow::checkpoint_ref`] by digest.

use std::sync::Arc;

use lash_durable::domain::{CellId, ExecKey, RunSeq, SessionCommitWrite, TurnWrite};
use lash_durable::runner::{Activation, Owned};
use lash_durable::{
    ActorTx, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableProbe, MailKind,
    Release,
};
use lash_sansio::SavedTurn;
use lash_sansio::sansio::{ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    ActorContext, AdmittedScope, Backend, Effect, ExecCodeFailure, ExecResponse, HostTurnProtocol,
    InputId, LlmCallError, LlmRequest, LlmResponse, Message, Response, SessionId,
    SessionStreamEvent, TurnId, TurnMachine, TurnMachineConfig, TurnOutcome,
};

pub use lash_durable::domain::{ModelPin, TurnCancelRequest, TurnPhase, TurnRow, TurnTerminal};

/// The mail kind a producer admits a turn with until L3s's (FIG-5196) mail
/// drain owns the session's mailbox: its body is
/// [`AdmittedInputs::mail_body`].
#[must_use]
pub fn admit_mail() -> MailKind {
    MailKind::new("session.admit")
}

/// What a turn is admitted with, as V0 encodes [`AdmittedInputs::admission_json`]:
/// the session head the turn's commit replaces and the messages the turn
/// starts from.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnAdmission {
    /// The head revision the turn's commit replaces.
    pub base_head: u64,
    /// The messages the turn starts from: the session's prompt and the
    /// admitted inputs.
    pub messages: Vec<Message>,
}

/// What a session's turns run with: the deployment's protocol, model,
/// environment and code cells. The phase runner calls each at its phase; none
/// of them drives a turn.
#[async_trait::async_trait]
pub trait TurnServices: Send + Sync {
    /// The configuration `session`'s turn `run` starts and restores under.
    fn machine_config(&self, session: &SessionId, run: &TurnId) -> TurnMachineConfig;

    /// The environment of the turn's next protocol iteration, recomputed
    /// from committed state.
    async fn sync_environment(
        &self,
        cx: &ActorContext,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure>;

    /// One attempt of a pinned model call.
    async fn call_model(
        &self,
        cx: &ActorContext,
        request: Arc<LlmRequest>,
        attempt: u32,
    ) -> Result<LlmResponse, LlmCallError>;

    /// Run the code cell `exec` from its latest snapshot, or from the start,
    /// to its end. `with` are the turn's rows that commit with the cell's
    /// first commit.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the cell could not reach its end; ownership loss
    /// among them.
    async fn exec_cell(
        &self,
        cx: &ActorContext,
        exec: ExecKey,
        language: &str,
        code: &str,
        with: Vec<DomainWrite>,
    ) -> Result<Result<ExecResponse, ExecCodeFailure>, TurnError>;
}

/// The session actor's activation: claim, drain mail, admit and run phases,
/// release.
#[derive(Clone)]
pub struct SessionActivation {
    backend: Backend,
    services: Arc<dyn TurnServices>,
    probe: Arc<dyn DurableProbe>,
}

impl std::fmt::Debug for SessionActivation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionActivation").finish_non_exhaustive()
    }
}

impl SessionActivation {
    /// The activation of sessions over `backend`, running turns with
    /// `services` and reporting to `probe` (`NoProbe` in production).
    #[must_use]
    pub fn new(
        backend: Backend,
        services: Arc<dyn TurnServices>,
        probe: Arc<dyn DurableProbe>,
    ) -> Self {
        Self {
            backend,
            services,
            probe,
        }
    }

    /// The backend it runs over.
    #[must_use]
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// One pass over the session's rows: admit a turn from its mail, run an
    /// unfinished turn to its commit, or release the actor.
    async fn pass(&self, cx: &ActorContext, session: &SessionId) -> Result<Pass, TurnError> {
        let mut tx = cx.begin().await?;
        let open = cx.durable_reads()?.turn(session).await?;
        let Some(row) = open else {
            let admission = tx
                .mail()
                .iter()
                .find(|mail| mail.kind == admit_mail())
                .map(|mail| (mail.seq, mail.body.clone()));
            return match admission {
                Some((seq, body)) => {
                    let inputs: AdmittedMail = serde_json::from_str(&body).map_err(|error| {
                        TurnError::Exec(format!("admission mail does not decode: {error}"))
                    })?;
                    admit_turn(cx, &mut tx, inputs.into()).await?;
                    tx.ack_through(seq);
                    cx.commit(tx, CommitLabel::TURN_ADMIT).await?;
                    Ok(Pass::Again)
                }
                None => {
                    tx.ack_seen().give_up(Release::Idle);
                    cx.commit(tx, CommitLabel::SESSION_RELEASE).await?;
                    Ok(Pass::Released)
                }
            };
        };
        drop(tx);
        let config = self.services.machine_config(session, &row.run);
        let turn = match row.checkpoint_ref {
            Some(_) => restore_turn(cx, config, &row).await?,
            None => start_turn(config, row)?,
        };
        match run_phases(cx, self.services.as_ref(), turn).await? {
            PhaseExit::Committed(_) => Ok(Pass::Again),
            PhaseExit::Lost => Ok(Pass::Lost),
            PhaseExit::Suspended { due } => {
                let mut tx = cx.begin().await?;
                tx.give_up(Release::Waiting { next_due: due });
                cx.commit(tx, CommitLabel::SESSION_RELEASE).await?;
                Ok(Pass::Released)
            }
        }
    }
}

/// What one activation pass left.
enum Pass {
    /// Run another pass.
    Again,
    /// The actor is released.
    Released,
    /// Ownership was lost.
    Lost,
}

/// [`AdmittedInputs`] as the admission mail encodes it.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AdmittedMail {
    run: TurnId,
    inputs: Vec<InputId>,
    admission_json: String,
}

impl From<AdmittedMail> for AdmittedInputs {
    fn from(mail: AdmittedMail) -> Self {
        Self {
            run: mail.run,
            inputs: mail.inputs,
            admission_json: mail.admission_json,
        }
    }
}

impl AdmittedInputs {
    /// The body of an [`admit_mail`] that admits these inputs.
    #[must_use]
    pub fn mail_body(&self) -> String {
        #[expect(
            clippy::expect_used,
            reason = "ids and a string encode without failure"
        )]
        serde_json::to_string(&AdmittedMail {
            run: self.run.clone(),
            inputs: self.inputs.clone(),
            admission_json: self.admission_json.clone(),
        })
        .expect("admission mail encodes")
    }
}

#[async_trait::async_trait]
impl Activation for SessionActivation {
    async fn activate(&self, owned: Owned) {
        let Ok(session) = SessionId::try_from(owned.actor().id().to_owned()) else {
            return;
        };
        let cx = ActorContext::claimed(
            self.backend.clone(),
            &owned,
            AdmittedScope::session_operation(session.clone(), "activation"),
            CancellationToken::new(),
            Arc::clone(&self.probe),
        );
        loop {
            match self.pass(&cx, &session).await {
                Ok(Pass::Again) => {}
                Ok(Pass::Released | Pass::Lost)
                | Err(TurnError::Durable(DurableError::OwnershipLost(_))) => return,
                // Anything else did not commit, or committed with its answer
                // lost: the next pass reloads the rows and carries on from
                // them. A lost epoch shows at its fenced read.
                Err(error) => {
                    tracing::debug!(%error, "session activation pass failed; reloading");
                    owned.wait_for_mail().await;
                }
            }
        }
    }
}

/// The inputs a drain hands the activation to admit as one turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedInputs {
    /// The run they open.
    pub run: TurnId,
    /// The inputs, in admission order.
    pub inputs: Vec<InputId>,
    /// The admission (base head revision, run spec, plugin revision),
    /// encoded by its owner.
    pub admission_json: String,
}

/// A turn restored from its rows: the machine, the effect it re-delivers,
/// and its row.
pub struct RestoredTurn {
    /// The machine, restored from the checkpoint.
    pub machine: TurnMachine,
    /// The effect the checkpoint re-delivers, if it is waiting on one.
    pub pending: Option<Effect>,
    /// The turn's row.
    pub row: TurnRow,
}

/// How a phase run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhaseExit {
    /// The turn committed with this terminal.
    Committed(TurnTerminal),
    /// Nothing is runnable: release as `waiting` until `due` or mail.
    Suspended {
        /// The earliest due time.
        due: Option<DurableInstant>,
    },
    /// Ownership was lost: drop everything held for the actor.
    Lost,
}

/// Why a turn was not admitted; nothing was recorded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TurnAdmitRefusal {
    /// The session already has an unfinished turn.
    #[error("session {0} already has an unfinished turn")]
    OpenTurnExists(SessionId),
}

/// Why a turn could not be restored from its rows.
#[derive(Debug, thiserror::Error)]
pub enum TurnRestoreError {
    /// The row names no checkpoint to restore from.
    #[error("turn {0} has no checkpoint")]
    NoCheckpoint(TurnId),
    /// The checkpoint does not restore.
    #[error(transparent)]
    Checkpoint(#[from] lash_sansio::TurnCheckpointRestoreError),
    /// The stored checkpoint does not decode.
    #[error("turn {run} checkpoint does not decode: {reason}")]
    Undecodable {
        /// The turn.
        run: TurnId,
        /// Why.
        reason: String,
    },
    /// The store refused.
    #[error(transparent)]
    Durable(#[from] DurableError),
}

/// Why a phase run failed.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// The store refused.
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// The runtime failed.
    #[error(transparent)]
    Runtime(#[from] crate::RuntimeError),
    /// The turn could not be admitted.
    #[error(transparent)]
    Admit(#[from] TurnAdmitRefusal),
    /// The turn could not be restored.
    #[error(transparent)]
    Restore(#[from] TurnRestoreError),
    /// An effect the turn yielded could not run, or its lane does not run it
    /// on this path yet.
    #[error("{0}")]
    Exec(String),
}

/// Admit `inputs` as the session's turn on `tx`: the turn row, the bound
/// inputs and the turn deadline (`turn.admit`).
///
/// The store refuses the commit with `OpenTurnExists` when the session has
/// an unfinished turn; binding the inputs' rows is L3s's (FIG-5196) drain,
/// and a host turn deadline is L3's.
///
/// # Errors
///
/// [`TurnAdmitRefusal`]; nothing is recorded.
pub async fn admit_turn(
    cx: &ActorContext,
    tx: &mut ActorTx,
    inputs: AdmittedInputs,
) -> Result<TurnRow, TurnAdmitRefusal> {
    let session = session_of(cx);
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: session.clone(),
        run: inputs.run.clone(),
        admission_json: inputs.admission_json.clone(),
        turn_deadline: None,
    }));
    Ok(TurnRow {
        session,
        run: inputs.run,
        admission_json: inputs.admission_json,
        phase: TurnPhase::Admitted,
        iteration: 0,
        checkpoint_ref: None,
        model: None,
        turn_deadline: None,
        written_epoch: cx.epoch(),
    })
}

#[expect(
    clippy::expect_used,
    reason = "a session actor's id is a session id by construction"
)]
fn session_of(cx: &ActorContext) -> SessionId {
    SessionId::try_from(cx.actor().id().to_owned()).expect("a session actor names its session")
}

/// A turn that has committed no checkpoint yet starts afresh from its
/// admission: nothing it did is durable, so nothing is restored.
fn start_turn(config: TurnMachineConfig, row: TurnRow) -> Result<RestoredTurn, TurnError> {
    let admission: TurnAdmission = serde_json::from_str(&row.admission_json)
        .map_err(|error| TurnError::Exec(format!("turn admission does not decode: {error}")))?;
    Ok(RestoredTurn {
        machine: TurnMachine::new(config, admission.messages, Default::default(), 0),
        pending: None,
        row,
    })
}

/// Restore `row`'s turn under `config`: exactly one
/// `restore_from_checkpoint`, reported to the context's probe, and the
/// effect it re-delivers.
///
/// # Errors
///
/// [`TurnRestoreError`].
pub async fn restore_turn(
    cx: &ActorContext,
    config: TurnMachineConfig,
    row: &TurnRow,
) -> Result<RestoredTurn, TurnRestoreError> {
    let stored = row
        .checkpoint_ref
        .as_deref()
        .ok_or_else(|| TurnRestoreError::NoCheckpoint(row.run.clone()))?;
    let saved: SavedTurn<HostTurnProtocol> =
        serde_json::from_str(stored).map_err(|error| TurnRestoreError::Undecodable {
            run: row.run.clone(),
            reason: error.to_string(),
        })?;
    cx.probe().checkpoint_restored(&row.session, &row.run);
    let mut machine = TurnMachine::restore_from_checkpoint(config, saved)?;
    let pending = next_work(&mut machine);
    Ok(RestoredTurn {
        machine,
        pending,
        row: row.clone(),
    })
}

/// The machine's next effect that needs work from the host, passing over
/// what only reports: emits, progress and logs. Their content is the
/// machine's own state, committed with its next checkpoint.
fn next_work(machine: &mut TurnMachine) -> Option<Effect> {
    loop {
        match machine.poll_effect()? {
            Effect::Emit(_) | Effect::Progress { .. } | Effect::Log { .. } => {}
            effect => return Some(effect),
        }
    }
}

fn encode_checkpoint(machine: &TurnMachine) -> Result<String, TurnError> {
    serde_json::to_string(&machine.checkpoint())
        .map_err(|error| TurnError::Exec(format!("the turn checkpoint does not encode: {error}")))
}

fn iteration(machine: &TurnMachine) -> u32 {
    u32::try_from(machine.protocol_iteration()).unwrap_or(u32::MAX)
}

/// Run the turn's phases from `turn`, committing at each label, until it
/// commits, suspends or loses ownership.
///
/// # Errors
///
/// [`TurnError`].
pub async fn run_phases(
    cx: &ActorContext,
    services: &dyn TurnServices,
    turn: RestoredTurn,
) -> Result<PhaseExit, TurnError> {
    let RestoredTurn {
        mut machine,
        mut pending,
        row,
    } = turn;
    let session = row.session.clone();
    let run = row.run.clone();
    let admission: TurnAdmission = serde_json::from_str(&row.admission_json)
        .map_err(|error| TurnError::Exec(format!("turn admission does not decode: {error}")))?;
    // The model call in flight as the rows left it: a re-delivered call is
    // its next attempt, under its recorded deadline.
    let mut model = match row.phase {
        TurnPhase::Model { .. } => row.model.clone().map(|pin| (row.iteration, pin)),
        _ => None,
    };
    let mut outcome = None;
    loop {
        let effect = match pending.take() {
            Some(effect) => effect,
            None => match machine.poll_effect() {
                Some(effect) => effect,
                None => return Err(TurnError::Exec("the turn machine stalled".to_owned())),
            },
        };
        match effect {
            Effect::Emit(SessionStreamEvent::TurnOutcome { outcome: ended }) => {
                outcome = Some(ended);
            }
            Effect::Emit(_) | Effect::Progress { .. } | Effect::Log { .. } => {}
            Effect::ReportToolCalls { .. } => {}
            Effect::SyncExecutionEnvironment { id } => {
                let result = services.sync_environment(cx, &session, &run).await;
                machine.handle_response(Response::ExecutionEnvironmentSynced { id, result });
            }
            Effect::Checkpoint { id, .. } => {
                // Plugin checkpoints and their deliveries are L3's: V0's
                // turn has none to deliver.
                machine.handle_response(Response::Checkpoint {
                    id,
                    delivery: Default::default(),
                });
            }
            Effect::LlmCall { id, request } => {
                let current = iteration(&machine);
                let pin = match model.take() {
                    Some((pinned, pin)) if pinned == current => ModelPin {
                        attempt: pin.attempt + 1,
                        ..pin
                    },
                    _ => {
                        let budget = lash_sansio::ExecutionBudgets::default().model_total();
                        let now = cx.durable_now().await?;
                        ModelPin {
                            attempt: 1,
                            request_ref: format!("checkpoint:effect/{}", id.0),
                            deadline: DurableInstant(now.0.saturating_add(
                                i64::try_from(budget.as_millis()).unwrap_or(i64::MAX),
                            )),
                        }
                    }
                };
                let mut tx = cx.begin().await?;
                tx.write(DomainWrite::Turn(TurnWrite::Advance {
                    session: session.clone(),
                    run: run.clone(),
                    phase: TurnPhase::Model {
                        attempt: pin.attempt,
                    },
                    iteration: current,
                    checkpoint_ref: Some(encode_checkpoint(&machine)?),
                    model: Some(pin.clone()),
                }));
                cx.commit(tx, CommitLabel::MODEL_START).await?;
                let attempt = pin.attempt;
                model = Some((current, pin));
                let result = services.call_model(cx, request, attempt).await;
                machine.handle_response(Response::LlmComplete {
                    id,
                    result,
                    text_streamed: false,
                });
            }
            Effect::ExecCode { id, language, code } => {
                model = None;
                let exec = ExecKey::Cell(
                    session.clone(),
                    run.clone(),
                    CellId::new(format!("e{}", id.0)),
                );
                let with = vec![DomainWrite::Turn(TurnWrite::Advance {
                    session: session.clone(),
                    run: run.clone(),
                    phase: TurnPhase::Tools { run: RunSeq(id.0) },
                    iteration: iteration(&machine),
                    checkpoint_ref: Some(encode_checkpoint(&machine)?),
                    model: None,
                })];
                let result = services.exec_cell(cx, exec, &language, &code, with).await?;
                machine.handle_response(Response::ExecResult { id, result });
            }
            Effect::ToolCalls { .. } | Effect::AwaitToolResults { .. } => {
                return Err(TurnError::Exec(
                    "tool rounds are L4's (FIG-5174) on the durable path".to_owned(),
                ));
            }
            Effect::Done { messages, .. } => {
                let terminal = match outcome {
                    Some(TurnOutcome::Finished(_)) => TurnTerminal::Answered,
                    Some(TurnOutcome::Stopped(crate::TurnStop::Cancelled { .. })) => {
                        TurnTerminal::Cancelled
                    }
                    _ => TurnTerminal::Failed,
                };
                let head = admission.base_head.saturating_add(1);
                let commit_json = serde_json::to_string(&messages.iter().collect::<Vec<_>>())
                    .map_err(|error| {
                        TurnError::Exec(format!("the turn's messages do not encode: {error}"))
                    })?;
                let mut tx = cx.begin().await?;
                tx.write(DomainWrite::SessionCommit(SessionCommitWrite {
                    session: session.clone(),
                    run: run.clone(),
                    expected_head: admission.base_head,
                    commit_json,
                }));
                tx.write(DomainWrite::Turn(TurnWrite::Terminal {
                    session: session.clone(),
                    run: run.clone(),
                    terminal,
                    cause_json: None,
                    head_revision: Some(head),
                }));
                cx.commit(tx, CommitLabel::TURN_COMMIT).await?;
                return Ok(PhaseExit::Committed(terminal));
            }
        }
    }
}

/// Request a cancel of `session`'s turn: mail plus a control wake.
///
/// # Errors
///
/// The store's refusal.
pub async fn request_turn_cancel(
    _backend: &Backend,
    _session: &SessionId,
    _request: TurnCancelRequest,
) -> Result<(), DurableError> {
    todo!("L3 (FIG-5172): request a turn cancel as session mail")
}
