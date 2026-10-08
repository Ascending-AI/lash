//! The facade's host head writes (FIG-4202): each is a session command on a
//! store-backed session, submitted under the writer and settled by the
//! shift at a turn boundary, and applied directly on a storeless one.

use super::*;

impl SessionAdmin {
    /// Submit `request`'s nodes to the session graph and return their status
    /// (FIG-4202).
    ///
    /// The bound turn owns a store-backed session's head, so the append is a
    /// session command the shift applies at the next turn boundary: the
    /// writer is held only to submit it, and the request's `operation_id` is
    /// its idempotency key. A storeless session appends directly under the
    /// writer, which already serializes the append with every turn it runs.
    pub(super) async fn append_session_nodes(
        &self,
        request: lash_core::AppendSessionNodesRequest,
    ) -> Result<AdminMutation<lash_core::AppendSessionNodesOutcome>> {
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
            SubmittedCommand::Applied(outcome) => Ok(AdminMutation::Applied(outcome)),
            SubmittedCommand::Queued(receipt) => {
                match Box::pin(self.command_status(receipt)).await? {
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::AppendSessionNodes { outcome },
                        ..
                    } => Ok(AdminMutation::Applied(outcome)),
                    lash_core::runtime::SessionCommandSettlement::Pending(receipt) => {
                        Ok(AdminMutation::Pending(receipt))
                    }
                    settlement => Err(unsettled_command_error(settlement)),
                }
            }
        }
    }

    /// Submit `request`'s frame open durably and return its status
    /// (FIG-4202): a session command on a store-backed session, applied and
    /// committed by the shift at the next turn boundary, under
    /// `idempotency_key`. A storeless session opens directly.
    pub(super) async fn open_agent_frame(
        &self,
        request: lash_core::OpenAgentFrameRequest,
        idempotency_key: String,
    ) -> Result<AdminMutation<lash_core::OpenAgentFrameOutcome>> {
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
            SubmittedCommand::Applied(outcome) => Ok(AdminMutation::Applied(outcome)),
            SubmittedCommand::Queued(receipt) => {
                match Box::pin(self.command_status(receipt)).await? {
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::OpenAgentFrame {
                                outcome:
                                    lash_core::runtime::OpenAgentFrameCommandOutcome::Opened { outcome },
                            },
                        ..
                    } => Ok(AdminMutation::Applied(outcome)),
                    lash_core::runtime::SessionCommandSettlement::Applied {
                        outcome:
                            lash_core::runtime::SessionCommandOutcome::OpenAgentFrame {
                                outcome:
                                    lash_core::runtime::OpenAgentFrameCommandOutcome::Refused {
                                        refusal,
                                    },
                            },
                        ..
                    } => Err(EmbedError::Runtime(refusal.into())),
                    lash_core::runtime::SessionCommandSettlement::Pending(receipt) => {
                        Ok(AdminMutation::Pending(receipt))
                    }
                    settlement => Err(unsettled_command_error(settlement)),
                }
            }
        }
    }

    /// Submit a tool-free plugin command at a session boundary under the
    /// host's stable key and return its status.
    pub(super) async fn run_plugin_command(
        &self,
        name: &str,
        args: serde_json::Value,
        idempotency_key: String,
    ) -> Result<AdminMutation<lash_core::facade_support::PluginOperationReceipt<serde_json::Value>>>
    {
        let session_id = SessionId::from(self.runtime.observe().session_id());
        let submitted = self
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
                match Box::pin(self.command_status(receipt)).await? {
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
    /// while no shift has admitted it. A command a shift already read, or
    /// that already settled, answers
    /// [`SessionCommandWithdrawal::AlreadyAdmitted`] and settles as that
    /// shift applies it. The withdrawal takes no runtime writer, so it never
    /// waits for the shift applying the session's commands (FIG-4391).
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

/// The error a command's caller answers when its settlement is not one its
/// command applies with: withdrawn, rejected before
/// acceptance, or another command's settlement shape.
pub(super) fn unsettled_command_error(
    settlement: lash_core::runtime::SessionCommandSettlement,
) -> EmbedError {
    match settlement {
        lash_core::runtime::SessionCommandSettlement::Cancelled(receipt) => {
            EmbedError::Session(SessionError::SessionCommandCancelled(receipt))
        }
        lash_core::runtime::SessionCommandSettlement::Rejected(error) => EmbedError::Runtime(error),
        // A command that could not apply settles with its typed failure.
        lash_core::runtime::SessionCommandSettlement::Applied {
            outcome: lash_core::runtime::SessionCommandOutcome::Failed { refusal },
            ..
        } => EmbedError::Runtime(refusal.into()),
        settlement => EmbedError::Session(SessionError::Protocol(format!(
            "a session command settled with another command's settlement: {settlement:?}"
        ))),
    }
}
