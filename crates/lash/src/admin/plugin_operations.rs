//! Keyed plugin commands, queries and task-run submission.

use super::*;

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
    pub(crate) control: SessionAdmin,
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
        let receipt = self
            .control
            .submit_session_command(
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
            self.control.target.clone(),
            operation.run_id(),
        ))
    }

    /// Run query `Op` over the plugin view a run or command published on
    /// this process. A query is not an admin read: it runs plugin code, so it
    /// needs the session's built plugins and never builds them (FIG-5139).
    /// Where none is published here (the session never ran or commanded on
    /// this process, as on a replica that has not served it) it is refused
    /// with [`PluginOperationInvokeError::NotPublished`](lash_core::facade_support::PluginOperationInvokeError::NotPublished):
    /// publish one with a session command, such as
    /// [`SessionCommandAdmin::refresh_tool_catalog`], then retry.
    pub async fn query<Op: lash_core::facade_support::PluginQuery>(
        &self,
        args: Op::Args,
    ) -> Result<Op::Output> {
        let (_plugin_id, output) = self
            .control
            .query_plugin_raw(Op::NAME, encode_plugin_args::<Op>(args)?)
            .await?;
        decode_plugin_output::<Op>(output)
    }

    /// [`Self::query`] by the query's registered name.
    pub async fn query_raw(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<(String, serde_json::Value)> {
        self.control.query_plugin_raw(name, args).await
    }

    pub async fn run_command<Op: lash_core::facade_support::PluginCommand>(
        &self,
        args: Op::Args,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<lash_core::facade_support::PluginOperationReceipt<Op::Output>>> {
        let receipt = Box::pin(self.control.run_plugin_command(
            Op::NAME,
            encode_plugin_args::<Op>(args)?,
            idempotency_key.into(),
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
        Box::pin(
            self.control
                .run_plugin_command(name, args, idempotency_key.into()),
        )
        .await
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
