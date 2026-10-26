//! Keyed plugin commands, queries and task-run submission.

use super::host_commands::unsettled_command_error;
use super::{AdminContext, AdminMutation, SessionCommandAdmin};
use crate::support::{EmbedError, LashRuntime, Result, SessionError};
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::RunIdCoreSupport;

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
    pub(crate) trace: lash_core::TraceCall,
}

impl PluginOperations {
    /// Open each durable invocation's `lash.plugin.invoke` producer span
    /// under `context`, the trace context of whatever caused it, instead of
    /// the caller's ambient one. The turns the operation queues retain the
    /// producer as their cause.
    pub fn trace_context(mut self, context: lash_core::TraceCarrier) -> Self {
        self.trace.caller = lash_core::TraceCaller::Explicit(Some(context));
        self
    }

    /// Open each invocation's producer span with no caller: a new trace
    /// root.
    pub fn trace_context_none(mut self) -> Self {
        self.trace.caller = lash_core::TraceCaller::Explicit(None);
        self
    }

    /// How each invocation's producer span relates to its caller: its
    /// child (the default), or a new trace root that links it.
    pub fn trace_link_mode(mut self, mode: lash_core::TraceLinkMode) -> Self {
        self.trace.link_mode = mode;
        self
    }

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
        .submit_traced(
            lash_core::facade_support::SessionCommand::RunPluginTask {
                name: name.into(),
                args,
            },
            idempotency_key,
            &self.trace,
        )
        .await?;
        let operation = lash_core::tool_run::OperationRun {
            session_id: receipt.session_id,
            operation_id: receipt.batch_id.to_string(),
        };
        Ok(crate::send::run(
            self.context.target.clone(),
            lash_vocabulary::identity::RunId::from_turn_id(operation.run_id()),
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
        // The invocation's producer opens here, before the first await.
        let producer = self.context.target.open_producer(
            lash_core::ProducerKind::PluginInvoke,
            &self.trace,
            lash_core::TraceIds {
                session: Some(session_id),
                ..Default::default()
            },
        );
        let parent = producer.parent();
        let submitted = self
            .context
            .with_writer(async |runtime: &mut LashRuntime| {
                let command = lash_core::facade_support::SessionCommand::RunPluginCommand {
                    name: name.to_string(),
                    args,
                };
                Box::pin(runtime.submit_session_command(command, idempotency_key, parent))
                    .await
                    .map_err(EmbedError::Runtime)
            })
            .await;
        let receipt = match submitted {
            Ok(receipt) => receipt,
            Err(error) => {
                producer.settle(crate::tracing::refused(&error));
                return Err(error);
            }
        };
        producer.settle(lash_core::ProducerOutcome::Selected);
        let receipt = match Box::pin(
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
                    .map(
                        |value| lash_execution_contracts::session_types::PluginOwned {
                            plugin_id: plugin_id.clone(),
                            value,
                        },
                    )
                    .collect(),
                pending_turn_inputs,
            },
            lash_core::runtime::SessionCommandSettlement::Applied {
                outcome:
                    lash_core::runtime::SessionCommandOutcome::PluginOperation {
                        outcome:
                            lash_core::runtime::PluginOperationCommandOutcome::Failed { failure },
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
                            lash_core::runtime::PluginOperationCommandOutcome::Refused { refusal },
                    },
                ..
            } => return Err(EmbedError::Runtime(refusal.into())),
            lash_core::runtime::SessionCommandSettlement::Applied {
                receipt,
                outcome:
                    lash_core::runtime::SessionCommandOutcome::PluginOperation {
                        outcome: lash_core::runtime::PluginOperationCommandOutcome::Cancelled,
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
        };
        self.record_plugin_operation_observations(&receipt.events, &receipt.pending_turn_inputs)
            .await;
        Ok(AdminMutation::Applied(receipt))
    }

    async fn record_plugin_operation_observations(
        &self,
        events: &[lash_execution_contracts::session_types::PluginOwned<
            lash_core::PluginRuntimeEvent,
        >],
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
        EmbedError::Plugin(lash_execution_contracts::plugin_error::PluginError::Invoke(
            format!("invalid {} args: {err}", Op::NAME),
        ))
    })
}

fn decode_plugin_output<Op: lash_core::facade_support::PluginOperation>(
    output: serde_json::Value,
) -> Result<Op::Output> {
    serde_json::from_value(output).map_err(|err| {
        EmbedError::Plugin(lash_execution_contracts::plugin_error::PluginError::Invoke(
            format!("invalid {} output: {err}", Op::NAME),
        ))
    })
}
