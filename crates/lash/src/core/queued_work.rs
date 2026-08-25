use super::build_plugin_host;
use crate::support::*;
use lash_core::facade_support;
use lash_core::facade_support::RuntimeSessionStateFacadeOps;

pub(crate) struct InlineQueuedWorkRunConfig {
    pub(super) session_execution_owner: lash_core::LeaseOwnerIdentity,
    pub(super) env: RuntimeEnvironment,
    pub(super) policy: SessionPolicy,
    pub(super) protocol_factory: Option<Arc<dyn PluginFactory>>,
    pub(super) plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    pub(super) store_factory: Arc<dyn SessionStoreFactory>,
    pub(super) live_replay_store: Arc<dyn LiveReplayStore>,
    pub(super) process_lifecycle_available: bool,
}

pub(super) struct InlineQueuedWorkRunHandle {
    config: Arc<InlineQueuedWorkRunConfig>,
}

impl InlineQueuedWorkRunHandle {
    pub(super) fn new(config: Arc<InlineQueuedWorkRunConfig>) -> Self {
        Self { config }
    }

    async fn drive_queued_work(
        &self,
        request: QueuedWorkRunRequest,
    ) -> std::result::Result<
        facade_support::QueuedWorkRunProgress,
        facade_support::QueuedWorkRunError,
    > {
        let Some(session_id) = request.session_id else {
            return Ok(facade_support::QueuedWorkRunProgress::Unknown);
        };
        let reason = request.reason;
        let mut policy = self.config.policy.clone();
        policy.session_id = Some(session_id.clone());
        let store = self
            .config
            .store_factory
            .create_store(&SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: SessionRelation::default(),
                policy: policy.clone(),
            })
            .await
            .map_err(|error| {
                facade_support::QueuedWorkRunError::terminal(lash_core::PluginError::Session(
                    error.to_string(),
                ))
            })?;
        let state = match crate::session::load_state_from_store(
            &session_id,
            &policy,
            store.as_ref(),
            &self.config.session_execution_owner,
            self.config.env.core.control.lease_timings.ttl_ms(),
        )
        .await
        {
            Ok(state) => state,
            Err(crate::EmbedError::Store(lash_core::StoreError::Contended)) => {
                return Ok(facade_support::QueuedWorkRunProgress::Blocked);
            }
            Err(error) => {
                return Err(facade_support::QueuedWorkRunError::terminal(
                    lash_core::PluginError::Session(error.to_string()),
                ));
            }
        };
        let plugin_host = build_plugin_host(
            self.config.protocol_factory.as_ref(),
            self.config.plugin_factories.as_ref(),
            Vec::new(),
        )
        .map_err(|error| {
            facade_support::QueuedWorkRunError::terminal(lash_core::PluginError::Session(
                error.to_string(),
            ))
        })?;
        let mut env = self.config.env.clone();
        env.core = plugin_host
            .install_process_engine_contributions(
                env.core.clone(),
                self.config.process_lifecycle_available,
            )
            .map_err(|error| {
                facade_support::QueuedWorkRunError::terminal(lash_core::PluginError::Session(
                    error.to_string(),
                ))
            })?;
        env.plugin_host = Some(Arc::new(plugin_host));
        let effect_host = Arc::clone(&env.core.control.effect_host);
        let runtime = LashRuntime::from_environment(
            &env,
            policy,
            state,
            Some(store),
            self.config.session_execution_owner.clone(),
        )
        .await
        .map_err(|error| {
            let error = match error {
                lash_core::SessionError::Plugin(error) => error,
                error => lash_core::PluginError::Session(error.to_string()),
            };
            facade_support::QueuedWorkRunError::terminal(error)
        })?;
        let handle = RuntimeHandle::with_live_replay_store(
            runtime,
            Arc::clone(&self.config.live_replay_store),
        );
        let mut claimed = false;
        loop {
            let scope = handle
                .observe()
                .persisted_state
                .queue_drain_scope(format!("{reason}:{}", uuid::Uuid::new_v4()));
            let scoped = effect_host.scoped(scope).map_err(|error| {
                facade_support::QueuedWorkRunError::terminal(lash_core::PluginError::Session(
                    error.to_string(),
                ))
            })?;
            let drain = crate::turn::stream_next_queued_prepared_turn(
                &handle,
                crate::turn::TurnSinks::default(),
                scoped,
                CancellationToken::new(),
                lash_core::TurnCancelOriginHint::default(),
            )
            .await
            .map_err(|error| {
                let plugin_error = lash_core::PluginError::Session(error.to_string());
                if error.is_retryable() {
                    facade_support::QueuedWorkRunError::transient(plugin_error)
                } else {
                    facade_support::QueuedWorkRunError::terminal(plugin_error)
                }
            })?;
            if let crate::turn::QueuedTurnDrain::Empty(empty_reason) = drain {
                tracing::debug!(
                    target: "lash::queued_work_run",
                    session_id = %session_id,
                    reason = empty_reason.as_str(),
                    claimed,
                    "inline queued-work run stopped on an empty drain"
                );
                return Ok(if claimed {
                    facade_support::QueuedWorkRunProgress::Claimed
                } else {
                    facade_support::QueuedWorkRunProgress::Blocked
                });
            }
            claimed = true;
        }
    }
}

#[async_trait]
impl QueuedWorkRunHandle for InlineQueuedWorkRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        session_id: Option<&str>,
    ) -> std::result::Result<Option<bool>, facade_support::QueuedWorkRunError> {
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        let mut policy = self.config.policy.clone();
        policy.session_id = Some(session_id.to_string());
        self.config
            .store_factory
            .has_claimable_queued_work(
                &SessionStoreCreateRequest {
                    session_id: session_id.to_string(),
                    relation: SessionRelation::default(),
                    policy,
                },
                self.config.env.core.clock.timestamp_ms(),
            )
            .await
            .map_err(|error| {
                facade_support::QueuedWorkRunError::terminal(lash_core::PluginError::Session(
                    error.to_string(),
                ))
            })
    }

    async fn run_queued_work(
        &self,
        request: QueuedWorkRunRequest,
    ) -> std::result::Result<(), facade_support::QueuedWorkRunError> {
        self.drive_queued_work(request).await?;
        Ok(())
    }

    async fn claim_and_run_pending_with_progress(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> std::result::Result<
        facade_support::QueuedWorkRunProgress,
        facade_support::QueuedWorkRunError,
    > {
        self.drive_queued_work(QueuedWorkRunRequest {
            session_id: session_id.map(str::to_string),
            reason: reason.to_string(),
            trace_idle: false,
        })
        .await
    }
}
