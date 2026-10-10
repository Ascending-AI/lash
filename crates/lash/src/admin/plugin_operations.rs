//! Keyed plugin commands, queries and task-run submission.

use super::host_commands::{SubmittedCommand, unsettled_command_error};
use super::{AdminContext, AdminMutation, SessionCommandAdmin};
use crate::support::{EmbedError, LashRuntime, Result, SessionError};
use lash_sansio::SessionId;

/// A task's decoded operation failure or a facade refusal. The complete
/// failure envelope retains classification, provenance and unknown data.
#[derive(Debug, thiserror::Error)]
pub enum PluginTaskResultError<Error> {
    /// The operation's declared error, decoded using its registered codec.
    #[error("plugin operation failed: {failure}")]
    Failed {
        error: Error,
        failure: Box<lash_core::plugin::PluginOperationFailure>,
    },
    /// Storage, cancellation, protocol, or an unrecognized operation failure.
    #[error(transparent)]
    Host(Box<EmbedError>),
}

impl<Error> From<PluginTaskResultError<Error>> for EmbedError {
    fn from(error: PluginTaskResultError<Error>) -> Self {
        match error {
            PluginTaskResultError::Failed { failure, .. } => Self::Control(
                lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
            ),
            PluginTaskResultError::Host(error) => *error,
        }
    }
}

#[derive(Clone)]
pub struct PluginOperations {
    pub(crate) context: AdminContext,
}

impl PluginOperations {
    /// Durably submit a host task under a stable key and return its operation
    /// Run. The engine owns execution; the handle follows, cancels and reads
    /// the result, including after a restart.
    pub async fn start_task<Op: lash_core::facade_support::PluginTask>(
        &self,
        args: Op::Args,
        idempotency_key: impl Into<String>,
    ) -> Result<crate::RunHandle<Op::Output, Op::Error>> {
        Ok(self
            .start_task_raw(Op::NAME, encode_plugin_args::<Op>(args)?, idempotency_key)
            .await?
            .typed::<Op>())
    }

    /// Submit a task by its registered name. Equal key and content reattach
    /// to the same operation Run.
    pub async fn start_task_raw(
        &self,
        name: &str,
        args: serde_json::Value,
        idempotency_key: impl Into<String>,
    ) -> Result<crate::RunHandle> {
        let receipt = SessionCommandAdmin {
            context: self.context.clone(),
        }
        .submit(
            lash_core::facade_support::SessionCommand::RunPluginTask {
                name: name.into(),
                args,
            },
            idempotency_key,
        )
        .await?;
        let operation = lash_core::tool_run::OperationRun {
            session_id: receipt.session_id,
            operation_id: receipt.batch_id.to_string(),
        };
        Ok(crate::send::run(
            self.context.target.clone(),
            operation.run_id(),
        ))
    }

    /// Run query `Op` over the plugin view the session's last run or command
    /// published, as its head records it. A query is not an admin read: it
    /// runs plugin code, on the plugins a run built on this handle or else
    /// on a private build of that recorded view, which installs and appends
    /// nothing (FIG-5344). Where the head records no published view (the
    /// session never ran or commanded) it is refused
    /// with [`PluginOperationInvokeError::NotPublished`](lash_core::facade_support::PluginOperationInvokeError::NotPublished):
    /// publish one with a session command, such as
    /// [`SessionCommandAdmin::refresh_tool_catalog`], then retry.
    pub async fn query<Op: lash_core::facade_support::PluginQuery>(
        &self,
        args: Op::Args,
    ) -> Result<Op::Output> {
        let (_plugin_id, output) = self
            .query_raw(Op::NAME, encode_plugin_args::<Op>(args)?)
            .await?;
        decode_plugin_output::<Op>(output)
    }

    /// [`Self::query`] by the query's registered name.
    pub async fn query_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(String, serde_json::Value)> {
        let observation = self.context.runtime.observe();
        let session_id = SessionId::from(observation.session_id());
        observation
            .query_plugin(name, args, Some(session_id))
            .await
            .map_err(Into::into)
    }

    pub async fn run_command<Op: lash_core::facade_support::PluginCommand>(
        &self,
        args: Op::Args,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<lash_core::facade_support::PluginOperationReceipt<Op::Output>>> {
        let receipt = Box::pin(self.run_command_raw(
            Op::NAME,
            encode_plugin_args::<Op>(args)?,
            idempotency_key,
        ))
        .await?;
        receipt.try_map(|receipt| {
            Ok(lash_core::facade_support::PluginOperationReceipt {
                output: decode_plugin_output::<Op>(receipt.output)?,
                events: receipt.events,
                pending_turn_inputs: receipt.pending_turn_inputs,
            })
        })
    }

    pub async fn run_command_raw(
        &self,
        name: &str,
        args: serde_json::Value,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<lash_core::facade_support::PluginOperationReceipt<serde_json::Value>>>
    {
        let idempotency_key = idempotency_key.into();
        let session_id = SessionId::from(self.context.runtime.observe().session_id());
        let submitted = self
            .context
            .with_writer(async |runtime: &mut LashRuntime| {
                if runtime.is_store_backed() {
                    let command = lash_core::facade_support::SessionCommand::RunPluginCommand {
                        name: name.to_string(),
                        args,
                    };
                    return Box::pin(runtime.submit_session_command(command, idempotency_key))
                        .await
                        .map(SubmittedCommand::Queued)
                        .map_err(EmbedError::Runtime);
                }
                let receipt = runtime
                    .run_storeless_plugin_command(name, args, Some(session_id.clone()))
                    .await;
                receipt.map(SubmittedCommand::Applied).map_err(Into::into)
            })
            .await?;
        let receipt = match submitted {
            SubmittedCommand::Applied(receipt) => receipt,
            SubmittedCommand::Queued(receipt) => {
                match Box::pin(
                    SessionCommandAdmin {
                        context: self.context.clone(),
                    }
                    .status(receipt),
                )
                .await?
                {
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::PluginOperation {
                                outcome:
                                    lash_core::runtime::PluginOperationCommandOutcome::Completed {
                                        plugin_id,
                                        output,
                                        events,
                                        pending_turn_inputs,
                                    },
                            },
                        ..
                    } => lash_core::facade_support::PluginOperationReceipt {
                        output,
                        events: events
                            .into_iter()
                            .map(|value| lash_core::facade_support::PluginOwned {
                                plugin_id: plugin_id.clone(),
                                value,
                            })
                            .collect(),
                        pending_turn_inputs,
                    },
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::PluginOperation {
                                outcome:
                                    lash_core::runtime::PluginOperationCommandOutcome::Failed {
                                        failure,
                                    },
                            },
                        ..
                    } => {
                        return Err(EmbedError::Control(
                            lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
                        ));
                    }
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::PluginOperation {
                                outcome:
                                    lash_core::runtime::PluginOperationCommandOutcome::Refused {
                                        refusal,
                                    },
                            },
                        ..
                    } => return Err(EmbedError::Runtime(refusal.into())),
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        receipt,
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::PluginOperation {
                                outcome:
                                    lash_core::runtime::PluginOperationCommandOutcome::Cancelled,
                            },
                    } => {
                        return Err(EmbedError::Session(SessionError::SessionCommandCancelled(
                            receipt,
                        )));
                    }
                    lash_core::runtime::SessionCommandSettlement::Pending(receipt) => {
                        return Ok(AdminMutation::Pending(receipt));
                    }
                    settlement => return Err(unsettled_command_error(settlement)),
                }
            }
        };
        self.record_plugin_operation_observations(&receipt.events, &receipt.pending_turn_inputs)
            .await;
        Ok(AdminMutation::Applied(receipt))
    }

    async fn record_plugin_operation_observations(
        &self,
        events: &[lash_core::facade_support::PluginOwned<lash_core::PluginRuntimeEvent>],
        pending_turn_inputs: &[lash_core::PendingTurnInput],
    ) {
        for owned in events {
            self.context
                .runtime
                .record_turn_activity(
                    None,
                    lash_core::TurnActivity::independent(lash_core::TurnEvent::PluginRuntime {
                        plugin_id: owned.plugin_id.clone(),
                        event: owned.value.clone(),
                    }),
                )
                .await;
        }
        if !pending_turn_inputs.is_empty() {
            self.context
                .runtime
                .record_queue_changed(
                    lash_core::SessionQueueEventKind::Enqueued,
                    pending_turn_inputs
                        .iter()
                        .map(|input| input.input_id.to_string())
                        .collect(),
                )
                .await;
        }
    }
}

fn encode_plugin_args<Op: lash_core::facade_support::PluginOperation>(
    args: Op::Args,
) -> Result<serde_json::Value> {
    serde_json::to_value(args).map_err(|err| {
        EmbedError::Plugin(lash_core::PluginError::Invoke(format!(
            "invalid {} args: {err}",
            Op::NAME
        )))
    })
}

fn decode_plugin_output<Op: lash_core::facade_support::PluginOperation>(
    output: serde_json::Value,
) -> Result<Op::Output> {
    serde_json::from_value(output).map_err(|err| {
        EmbedError::Plugin(lash_core::PluginError::Invoke(format!(
            "invalid {} output: {err}",
            Op::NAME
        )))
    })
}
