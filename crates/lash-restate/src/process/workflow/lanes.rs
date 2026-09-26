//! The generation lanes a segment passes before its admission (FIG-3795
//! S6, §4.4): the generation sentinel's refusal, and the input check that
//! holds a generation lane to its own generation and the stable lane to what
//! the newest build can run.

use lash_sansio::ProcessId;
use std::sync::Arc;

use lash_core::{PluginError, ProcessRegistration, ProcessRegistry};
use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerError, TerminalError};
use restate_sdk::serde::Json;

use super::super::{
    RestateProcessRunner, RestateProcessWorkflowInput, RestateProcessWorkflowPayload,
    handler_error_from_plugin,
};
use super::{LashProcessWorkflowImpl, step_fault};
use crate::controller::RestateControllerContext as _;
use crate::services::{Lane, ServiceRoute};

/// The journal name of the step that decides whether this build takes a
/// segment another build sent to the stable lane (FIG-3795 S6).
const SUCCESSOR_WINDOW_STEP: &str = "lash.segment.successor-window";

/// What the successor window decided for a segment another build sent to the
/// stable lane, journaled so a redrive takes the same branch.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "window", rename_all = "snake_case")]
enum SuccessorWindow {
    /// This build runs the segment: the newest build takes the next segment
    /// (the Temporal ruling).
    Admitted,
    /// This build cannot run it: the process parked `RetiredGeneration`,
    /// carrying the sender's generation, before the segment's start marker
    /// was written, so a re-send to that generation's lane admits it afresh.
    Refused { message: String },
}

impl<R> LashProcessWorkflowImpl<R>
where
    R: RestateProcessRunner,
{
    /// Refuse a journal the generation sentinel read back from another build
    /// (FIG-3795 §4.4): the code behind this invocation's pinned deployment
    /// was swapped. Nothing past the sentinel is replayed. The process parks
    /// `RetiredGeneration` carrying the recorded generation — best effort,
    /// through the root execution's authority, outside the journal, like a
    /// diverged segment's park — and the attempt ends the way a park ends,
    /// retryably, so the invocation keeps its journal for a build of the
    /// recorded generation.
    pub(super) async fn park_retired_journal(
        &self,
        process_id: Option<&ProcessId>,
        recorded: &lash_core::engine::BuildGeneration,
    ) -> HandlerError {
        let message = crate::sentinel::retired_generation_message(
            &self.route.name(),
            recorded,
            &self.build_generation,
        );
        if let Some(process_id) = process_id {
            let write = lash_core::store::ProcessParkWrite {
                reason: lash_core::store::ParkReason::RetiredGeneration {
                    generation: None,
                    message: message.clone(),
                },
                engine: None,
                build_generation: Some(recorded.clone()),
            };
            if let Err(error) = park_for_generation(&self.registry, process_id, write).await {
                tracing::error!(
                    event = "process.park_record_failed",
                    process_id = process_id.as_str(),
                    error = %error,
                    "a process whose journal another generation recorded could not record its park"
                );
            }
        }
        crate::parked_turn_failure(message)
    }

    /// The input check a segment passes after the generation sentinel and
    /// before any other command (FIG-3795 S6).
    ///
    /// - A generation lane serves only its own generation's work: an input
    ///   whose sender names another generation, or none, is a misroute and is
    ///   refused typed, terminally, without journaling anything more.
    /// - The stable lane runs what the newest build can run. A new process
    ///   (segment 0) or a segment this build sent itself runs. A later segment
    ///   another build handed over, or an input this build cannot decode,
    ///   passes the successor window first.
    pub(super) async fn admit_input(
        &self,
        ctx: &WorkflowContext<'_>,
        payload: RestateProcessWorkflowPayload,
    ) -> Result<RestateProcessWorkflowInput, HandlerError> {
        if let Lane::Generation(lane) = self.route.lane() {
            if payload.sender_generation() != Some(lane) {
                return Err(misrouted(&self.route, &payload));
            }
            return match payload {
                RestateProcessWorkflowPayload::Current(input) => Ok(*input),
                RestateProcessWorkflowPayload::Unreadable {
                    process_id, error, ..
                } => Err(undecodable(&self.route, process_id.as_ref(), &error)),
            };
        }
        match payload {
            RestateProcessWorkflowPayload::Current(input) => {
                let Some(sender) = input
                    .sender_generation
                    .clone()
                    .filter(|sender| *sender != self.build_generation)
                else {
                    return Ok(*input);
                };
                // A new process runs on the newest build, whoever started it.
                if input.segment_ordinal == 0 {
                    return Ok(*input);
                }
                match self
                    .successor_window(
                        ctx,
                        &input.process_id,
                        input.segment_ordinal,
                        &sender,
                        Some(&input.registration),
                        "",
                    )
                    .await?
                {
                    SuccessorWindow::Admitted => Ok(*input),
                    SuccessorWindow::Refused { message } => Err(refused_successor(message)),
                }
            }
            RestateProcessWorkflowPayload::Unreadable {
                process_id: Some(process_id),
                segment_ordinal,
                sender_generation: Some(sender),
                error,
            } => match self
                .successor_window(ctx, &process_id, segment_ordinal, &sender, None, &error)
                .await?
            {
                SuccessorWindow::Refused { message } => Err(refused_successor(message)),
                // No live process is left to park: the input names nothing
                // this build can run.
                SuccessorWindow::Admitted => {
                    Err(undecodable(&self.route, Some(&process_id), &error))
                }
            },
            RestateProcessWorkflowPayload::Unreadable {
                process_id, error, ..
            } => Err(undecodable(&self.route, process_id.as_ref(), &error)),
        }
    }

    /// Whether this build runs a segment another build sent to the stable
    /// lane (FIG-3795 S6), decided in one journaled step
    /// ([`SUCCESSOR_WINDOW_STEP`]).
    ///
    /// The newest build takes the next segment when it runs the executable
    /// generation the process's start recorded: the same fence the segment's
    /// admission holds it to (FIG-3571), taken here before the segment's
    /// start marker is written. A segment it cannot run — another executable
    /// generation, or an input it cannot decode — parks the process
    /// `RetiredGeneration` carrying the sender's generation, with no start
    /// marker and zero dispatch. The drain's re-send to that generation's
    /// lane (`LashProcessWorkflow_g<G>`) then admits and runs it there. The
    /// format read windows that widen this check are FIG-3802's.
    async fn successor_window(
        &self,
        ctx: &WorkflowContext<'_>,
        process_id: &ProcessId,
        segment_ordinal: u64,
        sender: &lash_core::engine::BuildGeneration,
        registration: Option<&ProcessRegistration>,
        decode_error: &str,
    ) -> Result<SuccessorWindow, HandlerError> {
        let current =
            registration.map(|registration| self.runner.executable_generation(registration));
        let registry = &self.registry;
        let own = &self.build_generation;
        let route = self.route.name();
        let Json(window) = ctx
            .run_json_or_retry_send::<Result<SuccessorWindow, String>, _>(
                SUCCESSOR_WINDOW_STEP.to_string(),
                async move {
                    let record = match registry.get_process(process_id).await {
                        Ok(Some(record)) => record,
                        // No process, or a terminal one: admission decides.
                        Ok(None) => return Ok(Ok(SuccessorWindow::Admitted)),
                        Err(error) => return step_fault(error),
                    };
                    if record.is_terminal() {
                        return Ok(Ok(SuccessorWindow::Admitted));
                    }
                    let reason = match current {
                        Some(current) => {
                            let recorded = record
                                .first_started
                                .as_deref()
                                .and_then(|started| started.generation.as_ref());
                            match lash_core::ExecutableGenerationRefusal::check(recorded, current) {
                                Ok(()) => return Ok(Ok(SuccessorWindow::Admitted)),
                                Err(refusal) => {
                                    lash_core::store::ParkReason::retired_process_generation(
                                        refusal,
                                    )
                                }
                            }
                        }
                        None => lash_core::store::ParkReason::RetiredGeneration {
                            generation: None,
                            message: format!(
                                "process `{process_id}` segment {segment_ordinal} was sent by \
                                 generation `{sender}` with an input this build (generation \
                                 `{own}`, {route}) does not decode: {decode_error}; its redrive \
                                 was refused before any effect; drain it to generation `{sender}`"
                            ),
                        },
                    };
                    let message = match &reason {
                        lash_core::store::ParkReason::RetiredGeneration { message, .. } => {
                            message.clone()
                        }
                        other => format!("{other:?}"),
                    };
                    let write = lash_core::store::ProcessParkWrite {
                        reason,
                        engine: None,
                        build_generation: Some(sender.clone()),
                    };
                    match park_for_generation(registry, process_id, write).await {
                        Ok(()) => Ok(Ok(SuccessorWindow::Refused { message })),
                        Err(error) => step_fault(error),
                    }
                },
            )
            .await
            .map_err(HandlerError::from)?;
        let window = window.map_err(TerminalError::new)?;
        if let SuccessorWindow::Refused { message } = &window {
            lash_core::operational_metrics::record_work_parked(
                "process",
                lash_core::store::ParkReasonCode::RetiredGeneration.as_str(),
            );
            tracing::warn!(
                event = "process.parked",
                process_id = process_id.as_str(),
                segment_ordinal,
                sender_generation = sender.as_str(),
                reason_code = lash_core::store::ParkReasonCode::RetiredGeneration.as_str(),
                message = message.as_str(),
                "a segment another build sent parked for its sender's generation"
            );
        }
        Ok(window)
    }
}

/// The typed refusal of an input a generation lane does not serve: its
/// sender names another generation, or none (FIG-3795 S6, law L11). Nothing
/// past the generation sentinel is journaled, and nothing is stored: a
/// misroute is the sender's error, never the process's outcome.
fn misrouted(route: &ServiceRoute, payload: &RestateProcessWorkflowPayload) -> HandlerError {
    let sender = payload.sender_generation().map_or_else(
        || "no generation".to_string(),
        |sender| format!("generation `{sender}`"),
    );
    let process = payload.process_id().map_or_else(
        || "an unreadable process".to_string(),
        |id| format!("process `{id}`"),
    );
    handler_error_from_plugin(PluginError::Runtime(lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
        format!(
            "misrouted: {route} serves only its own generation's segments; {process} segment {} \
             was sent by {sender}",
            payload.segment_ordinal()
        ),
    )))
}

/// The terminal refusal of an input this build cannot decode and cannot park
/// for its sender's generation.
fn undecodable(route: &ServiceRoute, process_id: Option<&ProcessId>, error: &str) -> HandlerError {
    let process = process_id.map_or_else(
        || "an unreadable process".to_string(),
        |id| format!("process `{id}`"),
    );
    TerminalError::new(format!(
        "{route} input for {process} does not decode in this build: {error}"
    ))
    .into()
}

/// The end of a stable-lane invocation whose successor window parked its
/// process for the sender's generation: terminal, because this build never
/// runs the segment — the drain's re-send to the sender's lane does.
fn refused_successor(message: String) -> HandlerError {
    TerminalError::new(message).into()
}

/// Park `process_id` for another generation outside any segment's own
/// admission, under the execution authority its root start recorded — the
/// identity the registry's same-execution fence checks, as the park reconcile
/// writes it. A process that never started names no execution to park under
/// and is left as it is.
async fn park_for_generation(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
    write: lash_core::store::ProcessParkWrite,
) -> Result<(), PluginError> {
    let Some(record) = registry.get_process(process_id).await? else {
        return Ok(());
    };
    let Some(authority) = super::super::park_reconcile::execution_authority(&record) else {
        tracing::warn!(
            process_id = process_id.as_str(),
            "a process that never started cannot park for another generation"
        );
        return Ok(());
    };
    registry
        .park_process_with_authority(process_id, write, &authority)
        .await
        .map(|_| ())
}
