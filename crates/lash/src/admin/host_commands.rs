//! The facade's host head writes (FIG-4202): each is a session command on a
//! store-backed session, submitted under the writer and settled by the
//! drive at a turn boundary, and applied directly on a storeless one.

use super::*;

impl SessionAdmin {
    /// Append `request`'s nodes to the session graph and await the append's
    /// settlement (FIG-4202).
    ///
    /// The bound turn owns a store-backed session's head, so the append is a
    /// session command the drive applies at the next turn boundary: the
    /// writer is held only to submit it, and the request's `operation_id` is
    /// its idempotency key. A storeless session appends directly under the
    /// writer, which already serializes the append with every turn it runs.
    pub(super) async fn append_session_nodes(
        &self,
        request: lash_core::AppendSessionNodesRequest,
    ) -> Result<lash_core::AppendSessionNodesOutcome> {
        let submitted = self
            .with_writer(async |runtime: &mut LashRuntime| {
                if runtime.is_store_backed() {
                    let idempotency_key = request.operation_id.clone();
                    return Box::pin(runtime.submit_session_command(
                        lash_core::facade_support::SessionCommand::AppendSessionNodes {
                            request: Box::new(request),
                        },
                        idempotency_key,
                    ))
                    .await
                    .map(SubmittedCommand::Queued)
                    .map_err(EmbedError::Runtime);
                }
                Box::pin(runtime.append_storeless_session_nodes(request))
                    .await
                    .map(SubmittedCommand::Applied)
                    .map_err(Into::into)
            })
            .await?;
        match submitted {
            SubmittedCommand::Applied(outcome) => Ok(outcome),
            SubmittedCommand::Queued(receipt) => {
                match Box::pin(self.await_command_settlement(receipt, None)).await? {
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::AppendSessionNodes { outcome },
                        ..
                    } => Ok(outcome),
                    settlement => Err(unsettled_command_error(settlement)),
                }
            }
        }
    }

    /// Open `request`'s frame durably and await the open's settlement
    /// (FIG-4202): a session command on a store-backed session, applied and
    /// committed by the drive at the next turn boundary, under
    /// `idempotency_key`. A storeless session opens directly.
    pub(super) async fn open_agent_frame(
        &self,
        request: lash_core::OpenAgentFrameRequest,
        idempotency_key: String,
    ) -> Result<lash_core::OpenAgentFrameOutcome> {
        let submitted = self
            .with_writer(async |runtime: &mut LashRuntime| {
                if runtime.is_store_backed() {
                    return Box::pin(runtime.submit_session_command(
                        lash_core::facade_support::SessionCommand::OpenAgentFrame {
                            request: Box::new(request),
                        },
                        idempotency_key,
                    ))
                    .await
                    .map(SubmittedCommand::Queued)
                    .map_err(EmbedError::Runtime);
                }
                Box::pin(runtime.open_storeless_agent_frame(request))
                    .await
                    .map(SubmittedCommand::Applied)
                    .map_err(EmbedError::Runtime)
            })
            .await?;
        match submitted {
            SubmittedCommand::Applied(outcome) => Ok(outcome),
            SubmittedCommand::Queued(receipt) => {
                match Box::pin(self.await_command_settlement(receipt, None)).await? {
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::OpenAgentFrame {
                                outcome:
                                    lash_core::runtime::OpenAgentFrameCommandOutcome::Opened { outcome },
                            },
                        ..
                    } => Ok(outcome),
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::OpenAgentFrame {
                                outcome:
                                    lash_core::runtime::OpenAgentFrameCommandOutcome::Refused {
                                        code,
                                        message,
                                    },
                            },
                        ..
                    } => Err(EmbedError::Runtime(lash_core::RuntimeError::new(
                        code, message,
                    ))),
                    settlement => Err(unsettled_command_error(settlement)),
                }
            }
        }
    }

    /// Run a host plugin command or task and await its settlement
    /// (FIG-4202).
    ///
    /// On a store-backed session the operation is a session command: the
    /// writer is held only to submit it, and the plugin's code runs in the
    /// drive at the next turn boundary, its events, state and queued turns
    /// settling with the command. `cancellation` withdraws a command no drive
    /// has admitted yet. A task a drive already admitted is cancelled through
    /// its cancel gate (FIG-4391): its drive stops the task's code and
    /// settles it cancelled, unless the task's code returned first; an
    /// admitted plugin command runs to its settlement. A storeless session
    /// runs the operation directly under the writer.
    pub(super) async fn run_plugin_operation(
        &self,
        operation: HostPluginOperation,
        name: &str,
        args: serde_json::Value,
        cancellation: CancellationToken,
    ) -> Result<lash_core::facade_support::PluginOperationReceipt<serde_json::Value>> {
        let session_id = SessionId::from(self.runtime.observe().session_id());
        let submitted = self
            .with_writer(async |runtime: &mut LashRuntime| {
                if runtime.is_store_backed() {
                    let command = match operation {
                        HostPluginOperation::Command => {
                            lash_core::facade_support::SessionCommand::RunPluginCommand {
                                name: name.to_string(),
                                args,
                            }
                        }
                        HostPluginOperation::Task => {
                            lash_core::facade_support::SessionCommand::RunPluginTask {
                                name: name.to_string(),
                                args,
                            }
                        }
                    };
                    let idempotency_key =
                        format!("{}:{name}:{}", command.kind(), uuid::Uuid::new_v4());
                    return Box::pin(runtime.submit_session_command(command, idempotency_key))
                        .await
                        .map(SubmittedCommand::Queued)
                        .map_err(EmbedError::Runtime);
                }
                let receipt = match operation {
                    HostPluginOperation::Command => {
                        runtime
                            .run_storeless_plugin_command(name, args, Some(session_id.clone()))
                            .await
                    }
                    HostPluginOperation::Task => {
                        let scope = lash_core::ExecutionScope::runtime_operation(format!(
                            "{session_id}:plugin_task:{name}:{}",
                            uuid::Uuid::new_v4()
                        ));
                        let controller = runtime
                            .effect_host()
                            .scoped_static(lash_core::AdmittedScope::new(scope))
                            .map_err(EmbedError::Runtime)?
                            .ok_or_else(|| {
                                EmbedError::Plugin(lash_core::PluginError::Session(
                                    "plugin task execution requires an effect host that can \
                                     create a static runtime-operation scope"
                                        .to_string(),
                                ))
                            })?;
                        runtime
                            .run_storeless_plugin_task(
                                name,
                                args,
                                Some(session_id.clone()),
                                controller,
                                cancellation.clone(),
                            )
                            .await
                    }
                };
                receipt.map(SubmittedCommand::Applied).map_err(Into::into)
            })
            .await?;
        let receipt = match submitted {
            SubmittedCommand::Applied(receipt) => receipt,
            SubmittedCommand::Queued(receipt) => {
                match Box::pin(self.settle_or_withdraw(receipt, operation, cancellation)).await? {
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
                                        message,
                                    },
                            },
                        ..
                    } => {
                        return Err(EmbedError::Plugin(lash_core::PluginError::Invoke(message)));
                    }
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
                    settlement => return Err(unsettled_command_error(settlement)),
                }
            }
        };
        self.record_plugin_operation_observations(&receipt.events, &receipt.pending_turn_inputs);
        Ok(receipt)
    }

    /// Await the settlement of the plugin `operation` `receipt` names; if
    /// `cancellation` fires first, cancel it. A command no drive admitted is
    /// withdrawn transactionally and answers `Cancelled` (FIG-4202). One a
    /// drive already admitted is settled by that drive, and its settlement is
    /// awaited: for a task, after its cancel gate was resolved cancelled, so
    /// the drive stops the task's code and settles it cancelled unless the
    /// task's code returned first (FIG-4391).
    pub(super) async fn settle_or_withdraw(
        &self,
        receipt: lash_core::runtime::SessionCommandReceipt,
        operation: HostPluginOperation,
        cancellation: CancellationToken,
    ) -> Result<lash_core::runtime::SessionCommandSettlement> {
        tokio::select! {
            settled = Box::pin(self.await_command_settlement(receipt.clone(), None)) => settled,
            () = cancellation.cancelled() => {
                match self.withdraw_session_command(&receipt).await? {
                    SessionCommandWithdrawal::Withdrawn => {
                        Ok(lash_core::runtime::SessionCommandSettlement::Cancelled(receipt))
                    }
                    SessionCommandWithdrawal::AlreadyAdmitted => {
                        if operation == HostPluginOperation::Task {
                            self.cancel_admitted_plugin_task(&receipt).await?;
                        }
                        Box::pin(self.await_command_settlement(receipt, None)).await
                    }
                }
            }
        }
    }

    /// Resolve the cancel gate of the admitted plugin task `receipt` names
    /// (FIG-4391), without the runtime's writer, which the drive applying the
    /// task holds. Whether the cancel won the gate or the task's code
    /// returned first, the command's settlement says how it ended.
    async fn cancel_admitted_plugin_task(
        &self,
        receipt: &lash_core::runtime::SessionCommandReceipt,
    ) -> Result<lash_core::runtime::PluginTaskCancelRequest> {
        self.require_own_command(receipt)?;
        lash_core::runtime::request_plugin_task_cancel(&self.runtime.observe().effect_host, receipt)
            .await
            .map_err(EmbedError::Runtime)
    }

    /// Refuse a host operation on a command of another session than this
    /// one.
    fn require_own_command(
        &self,
        receipt: &lash_core::runtime::SessionCommandReceipt,
    ) -> Result<()> {
        let session_id = SessionId::from(self.runtime.observe().session_id());
        if session_id != receipt.session_id {
            return Err(EmbedError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::StoreCommitFailed,
                lash_core::StoreError::ForeignSessionRequest {
                    view_session_id: session_id,
                    request_session_id: receipt.session_id.clone(),
                }
                .to_string(),
            )));
        }
        Ok(())
    }

    /// Withdraw the command `receipt` names (FIG-4202): transactionally,
    /// while no drive has admitted it. A command a drive already read, or
    /// that already settled, answers
    /// [`SessionCommandWithdrawal::AlreadyAdmitted`] and settles as that
    /// drive applies it. The withdrawal takes no runtime writer, so it never
    /// waits for the drive applying the session's commands (FIG-4391).
    pub(super) async fn withdraw_session_command(
        &self,
        receipt: &lash_core::runtime::SessionCommandReceipt,
    ) -> Result<SessionCommandWithdrawal> {
        self.require_own_command(receipt)?;
        let withdrawn = self
            .runtime
            .cancel_queued_work_batch(receipt.batch_id.as_str())
            .await
            .map_err(EmbedError::Runtime)?;
        Ok(match withdrawn {
            Some(_) => SessionCommandWithdrawal::Withdrawn,
            None => SessionCommandWithdrawal::AlreadyAdmitted,
        })
    }
}

/// A host head write as its submission left it (FIG-4201, FIG-4202):
/// queued on a store-backed session's command lane, or applied directly on
/// a storeless one, with what it applied as.
pub(super) enum SubmittedCommand<T> {
    Queued(lash_core::runtime::SessionCommandReceipt),
    Applied(T),
}

/// Which host plugin operation the facade runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostPluginOperation {
    Command,
    Task,
}

/// The error a command's caller answers when its settlement is not one its
/// command applies with: still pending, withdrawn, rejected before
/// acceptance, or another command's settlement shape.
pub(super) fn unsettled_command_error(
    settlement: lash_core::runtime::SessionCommandSettlement,
) -> EmbedError {
    match settlement {
        lash_core::runtime::SessionCommandSettlement::Pending(receipt) => {
            EmbedError::Session(SessionError::SessionCommandPending(receipt))
        }
        lash_core::runtime::SessionCommandSettlement::Cancelled(receipt) => {
            EmbedError::Session(SessionError::SessionCommandCancelled(receipt))
        }
        lash_core::runtime::SessionCommandSettlement::Rejected(error) => EmbedError::Runtime(error),
        // A command that could not apply settles with its typed failure.
        lash_core::runtime::SessionCommandSettlement::Applied {
            outcome: lash_core::runtime::SessionCommandOutcome::Failed { code, message },
            ..
        } => EmbedError::Runtime(lash_core::RuntimeError::new(code, message)),
        settlement => EmbedError::Session(SessionError::Protocol(format!(
            "a session command settled with another command's settlement: {settlement:?}"
        ))),
    }
}
