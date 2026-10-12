use super::host_commands::{SubmittedCommand, unsettled_command_error};
use super::{AdminContext, AdminMutation, SessionCommandAdmin};
use crate::support::{
    Arc, EmbedError, LashRuntime, PluginMessage, Result, RuntimeSessionState, SessionStateService,
};
use lash_sansio::SessionId;

#[derive(Clone)]
/// Facade handle for session state administration.
pub struct SessionStateAdmin {
    pub(super) context: AdminContext,
}

impl SessionStateAdmin {
    /// Export this handle's resident state. Read the committed head explicitly
    /// with [`DurableSession::read`](crate::DurableSession::read) or
    /// [`ObservableSession::snapshot`](crate::ObservableSession::snapshot).
    pub async fn export(&self) -> lash_core::SessionSnapshot {
        self.context.runtime.observe().read_view.to_snapshot()
    }

    pub async fn append_messages(
        &self,
        messages: Vec<PluginMessage>,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<()>> {
        let idempotency_key = idempotency_key.into();
        Box::pin(
            self.append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: idempotency_key,
                nodes: messages
                    .into_iter()
                    .map(lash_core::SessionAppendNode::message)
                    .collect(),
                requires_ancestor_node_id: None,
            }),
        )
        .await
        .and_then(|outcome| outcome.try_map(|_| Ok(())))
    }

    pub async fn append_plugin_body(
        &self,
        plugin_type: impl Into<String>,
        body: serde_json::Value,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<()>> {
        let idempotency_key = idempotency_key.into();
        Box::pin(
            self.append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: idempotency_key,
                nodes: vec![lash_core::SessionAppendNode::plugin(plugin_type, body)],
                requires_ancestor_node_id: None,
            }),
        )
        .await
        .and_then(|outcome| outcome.try_map(|_| Ok(())))
    }

    /// Submit a keyed append to the session command lane. A pending result
    /// retains its receipt for a host-chosen settlement wait.
    pub async fn append_session_nodes(
        &self,
        request: lash_core::AppendSessionNodesRequest,
    ) -> Result<AdminMutation<lash_core::AppendSessionNodesOutcome>> {
        let submitted = self
            .context
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
    /// (FIG-4202): a session command, keyed by `idempotency_key`, that the
    /// session's shift opens and commits at the next turn boundary,
    /// restarting its live interpreter from the frame's seed. A refused open
    /// answers its typed runtime error.
    pub async fn open_agent_frame(
        &self,
        request: lash_core::OpenAgentFrameRequest,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<lash_core::OpenAgentFrameOutcome>> {
        let idempotency_key = idempotency_key.into();
        let submitted = self
            .context
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

    /// Replaces resident state WITHOUT durable publication; test and recovery
    /// tooling only, never a product path.
    #[cfg(any(test, feature = "testing"))]
    pub async fn set_persisted(&self, state: RuntimeSessionState) -> Result<()> {
        self.context
            .with_writer(async |runtime: &mut LashRuntime| {
                runtime.apply_persistence_state(state).map_err(Into::into)
            })
            .await
    }

    /// Persists and returns the session's current runtime state.
    pub async fn persist_current(&self) -> Result<RuntimeSessionState> {
        self.context
            .with_writer(async |runtime: &mut LashRuntime| {
                runtime.await_background_work().await?;
                runtime.export_persisted_state().await.map_err(Into::into)
            })
            .await
    }

    pub async fn session_state_service(&self) -> Result<Arc<dyn SessionStateService>> {
        self.context
            .runtime
            .writer()
            .lock()
            .await
            .session_state_service()
            .map_err(Into::into)
    }

    /// The protocol execution state (root and leaves) the session's durable
    /// head recorded, read from the store without building the session's
    /// capabilities (FIG-5139): the state the head's last commit captured,
    /// on any process, whether or not a run built this handle's runtime.
    /// `None` when the head records none, which includes a protocol without
    /// a code executor and a frame switch that cleared it.
    ///
    /// Execution state moves only with the session's history: a host seeds
    /// a fresh interpreter by opening a frame with a seed
    /// ([`Self::open_agent_frame`]), never by writing a snapshot over the
    /// head.
    pub async fn snapshot_execution(
        &self,
    ) -> Result<Option<lash_core::plugin::HydratedExecutionGraph>> {
        let store = self.context.head_store()?;
        let Some(loaded) = lash_core::store::load_session_window_state(
            &store,
            lash_core::store::WindowSelector::Current,
        )
        .await
        .map_err(EmbedError::Store)?
        else {
            return Ok(None);
        };
        loaded
            .state
            .execution_state_hydration()
            .map_err(EmbedError::Store)
    }

    /// Submit a keyed compaction. An applied result says whether a frame opened;
    /// a pending result carries the durable receipt for settlement.
    pub async fn compact_context(
        &self,
        instructions: Option<String>,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<bool>> {
        let idempotency_key = idempotency_key.into();
        let submitted = self
            .context
            .with_writer(async |runtime: &mut LashRuntime| {
                if runtime.is_store_backed() {
                    return Box::pin(runtime.submit_session_command(
                        lash_core::facade_support::SessionCommand::CompactContext { instructions },
                        idempotency_key,
                    ))
                    .await
                    .map(SubmittedCommand::Queued)
                    .map_err(EmbedError::Runtime);
                }
                let host = runtime.effect_host();
                let controller = host
                    .scoped(lash_core::AdmittedScope::session_operation(
                        SessionId::from(runtime.session_id()),
                        idempotency_key,
                    ))
                    .map_err(EmbedError::Runtime)?;
                Box::pin(runtime.compact_storeless_context(instructions, controller))
                    .await
                    .map(SubmittedCommand::Applied)
                    .map_err(EmbedError::Runtime)
            })
            .await?;
        let outcome = match submitted {
            SubmittedCommand::Applied(outcome) => outcome,
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
                            lash_core::runtime::SessionCommandOutcome::CompactContext { outcome },
                        ..
                    } => outcome,
                    lash_core::runtime::SessionCommandSettlement::Pending(receipt) => {
                        return Ok(AdminMutation::Pending(receipt));
                    }
                    settlement => return Err(unsettled_command_error(settlement)),
                }
            }
        };
        match outcome {
            lash_core::runtime::CompactContextOutcome::Opened { .. } => {
                Ok(AdminMutation::Applied(true))
            }
            lash_core::runtime::CompactContextOutcome::NothingToCompact => {
                Ok(AdminMutation::Applied(false))
            }
            lash_core::runtime::CompactContextOutcome::Failed { refusal } => {
                Err(EmbedError::Runtime(refusal.into()))
            }
        }
    }

    /// Refresh the session graph from durable state
    /// changed. This is the honest name for what the core `await_background_work`
    /// call does — a session-graph resync, **not** a terminal wait on background
    /// work (that lives on the process admin's `await_output`). Renamed off the
    /// old `SessionProcessAdmin::await_all` misnomer per the ADR 0014 grill.
    pub(crate) async fn refresh_background_graph(&self) -> Result<()> {
        self.context
            .with_writer(async |runtime: &mut LashRuntime| {
                runtime.await_background_work().await.map_err(Into::into)
            })
            .await
    }
}
