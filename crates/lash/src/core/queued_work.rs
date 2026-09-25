use super::build_plugin_host;
use crate::support::{
    Arc, LashRuntime, LiveReplayStore, PluginFactory, QueuedWorkRunHandle, QueuedWorkRunRequest,
    RuntimeEnvironment, RuntimeHandle, SessionPolicy, SessionRelation, SessionStoreCreateRequest,
    SessionStoreFactory, async_trait,
};
use lash_core::facade_support;
use lash_sansio::SessionId;

pub(crate) struct NativeQueuedWorkRunConfig {
    pub(super) session_execution_owner: lash_core::LeaseOwnerIdentity,
    pub(super) env: RuntimeEnvironment,
    pub(super) policy: SessionPolicy,
    pub(super) protocol_factory: Option<Arc<dyn PluginFactory>>,
    pub(super) plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    pub(super) store_factory: Arc<dyn SessionStoreFactory>,
    pub(super) live_replay_store: Arc<dyn LiveReplayStore>,
    pub(super) process_lifecycle_available: bool,
}

/// The driver is wired inside the core's port setup, where a test cannot reach
/// it; this assembles the same config from the same core so a test can drive
/// one batch directly and read the failure class it produces.
#[cfg(test)]
pub(crate) fn native_queued_work_handle_for_tests(
    core: &crate::LashCore,
    store_factory: Arc<dyn SessionStoreFactory>,
) -> NativeQueuedWorkRunHandle {
    NativeQueuedWorkRunHandle::new(Arc::new(NativeQueuedWorkRunConfig {
        session_execution_owner: core.session_execution_owner.clone(),
        env: core.env.clone(),
        policy: core.policy.clone(),
        protocol_factory: core.protocol_factory.clone(),
        plugin_factories: Arc::clone(&core.plugin_factories),
        store_factory,
        live_replay_store: Arc::clone(&core.live_replay_store),
        process_lifecycle_available: core.process_lifecycle_available,
    }))
}

/// The core's session driver (FIG-3600): opens a session's runtime with the
/// core's plugins and runs the kernel's drive on it.
///
/// It is installed on the backend's session-work engine at core build. On the
/// in-process engine that SQLite and PostgreSQL still use until FIG-3668, it
/// is also that engine's run handle: each claimed session is driven to a stop
/// under a fresh drive request.
pub(crate) struct NativeQueuedWorkRunHandle {
    config: Arc<NativeQueuedWorkRunConfig>,
    next_request: std::sync::atomic::AtomicU64,
}

impl NativeQueuedWorkRunHandle {
    pub(crate) fn new(config: Arc<NativeQueuedWorkRunConfig>) -> Self {
        Self {
            config,
            next_request: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// A drive request unique to this process incarnation: the in-process
    /// engine coalesces asks per session, so each run it starts is a new
    /// drive whose admissions nothing else replays.
    fn native_request(&self, session_id: &SessionId) -> lash_core::engine::DriveRequest {
        let ordinal = self
            .next_request
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        lash_core::engine::DriveRequest {
            session: session_id.clone(),
            request: lash_core::engine::DriveRequestId::new(format!(
                "native:{}:{ordinal}",
                self.config.session_execution_owner.incarnation_id
            )),
            build_generation: lash_core::engine::BuildGeneration::new(""),
        }
    }

    /// Open `session_id`'s runtime with the core's plugins.
    async fn open_runtime(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(RuntimeHandle, Arc<dyn lash_core::EffectHost>), OpenFailure> {
        let mut policy = self.config.policy.clone();
        policy.session_id = Some(session_id.clone());
        let store = self
            .config
            .store_factory
            .create_store(&SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: SessionRelation::default(),
                policy: policy.clone(),
            })
            .await
            .map_err(|error| {
                OpenFailure::Terminal(lash_core::PluginError::Session(error.to_string()))
            })?;
        let state = match crate::session::load_state_from_store(
            session_id,
            &policy,
            store.as_ref(),
            &self.config.session_execution_owner,
            self.config.env.core.control.lease_timings.ttl_ms(),
        )
        .await
        {
            Ok(state) => state,
            Err(crate::EmbedError::Store(lash_core::StoreError::Contended)) => {
                return Err(OpenFailure::Contended);
            }
            Err(error) => {
                return Err(OpenFailure::Terminal(lash_core::PluginError::Session(
                    error.to_string(),
                )));
            }
        };
        let plugin_host = build_plugin_host(
            self.config.protocol_factory.as_ref(),
            self.config.plugin_factories.as_ref(),
            Vec::new(),
        )
        .map_err(|error| {
            OpenFailure::Terminal(lash_core::PluginError::Session(error.to_string()))
        })?;
        let mut env = self.config.env.clone();
        env.core = plugin_host
            .install_process_engine_contributions(
                env.core.clone(),
                self.config.process_lifecycle_available,
            )
            .map_err(|error| {
                OpenFailure::Terminal(lash_core::PluginError::Session(error.to_string()))
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
            OpenFailure::Terminal(match error {
                lash_core::SessionError::Plugin(error) => error,
                error => lash_core::PluginError::Session(error.to_string()),
            })
        })?;
        let handle = RuntimeHandle::with_live_replay_store(
            runtime,
            Arc::clone(&self.config.live_replay_store),
        );
        Ok((handle, effect_host))
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
        let drive = self.native_request(&session_id);
        match lash_core::SessionDriver::drive(self, drive).await {
            Ok(outcome) => {
                tracing::debug!(
                    target: "lash::queued_work_run",
                    session_id = %session_id,
                    ran = outcome.ran.len(),
                    stop = ?outcome.stop,
                    "native session drive stopped"
                );
                Ok(if outcome.ran.is_empty() {
                    facade_support::QueuedWorkRunProgress::Blocked
                } else {
                    facade_support::QueuedWorkRunProgress::Claimed
                })
            }
            Err(abort) => {
                let retry = matches!(abort, lash_core::engine::DriveAbort::Retry(_));
                let error = lash_core::PluginError::Session(abort.to_string());
                Err(if retry {
                    facade_support::QueuedWorkRunError::transient(error)
                } else {
                    facade_support::QueuedWorkRunError::terminal(error)
                })
            }
        }
    }
}

/// Why a session's runtime did not open for a drive.
enum OpenFailure {
    /// Another writer holds the session; the drive is retried.
    Contended,
    Terminal(lash_core::PluginError),
}

impl OpenFailure {
    fn into_abort(self) -> lash_core::engine::DriveAbort {
        match self {
            Self::Contended => lash_core::engine::DriveAbort::Retry(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::StoreCommitContended,
                "the session's runtime is contended; the drive is retried",
            )),
            Self::Terminal(error) => {
                lash_core::engine::DriveAbort::Refused(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::PluginSessionManager,
                    error.to_string(),
                ))
            }
        }
    }
}

#[async_trait]
impl lash_core::SessionDriver for NativeQueuedWorkRunHandle {
    async fn drive(
        &self,
        request: lash_core::engine::DriveRequest,
    ) -> std::result::Result<lash_core::engine::DriveOutcome, lash_core::engine::DriveAbort> {
        let (handle, effect_host) = self
            .open_runtime(&request.session)
            .await
            .map_err(OpenFailure::into_abort)?;
        let controller = effect_host
            .scoped(lash_core::engine::drive_admission_scope(
                &request.session,
                &request.request,
            ))
            .map_err(lash_core::engine::DriveAbort::Refused)?;
        crate::turn::drive_session_observed(&handle, &controller, &request).await
    }

    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &lash_core::engine::DriveRequest,
        ordinal: u32,
    ) -> std::result::Result<lash_core::engine::AdmitVerdict, lash_core::engine::DriveAbort> {
        let (handle, _) = self
            .open_runtime(&request.session)
            .await
            .map_err(OpenFailure::into_abort)?;
        crate::turn::admit_drive_observed(&handle, &controller, request, ordinal).await
    }

    async fn run_root(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        admitted: lash_core::engine::Admitted,
    ) -> std::result::Result<lash_core::engine::RootOutcome, lash_core::engine::DriveAbort> {
        let (handle, _) = self
            .open_runtime(admitted.session())
            .await
            .map_err(OpenFailure::into_abort)?;
        crate::turn::run_admitted_root_observed(&handle, &controller, admitted).await
    }
}

#[async_trait]
impl QueuedWorkRunHandle for NativeQueuedWorkRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        session_id: Option<&SessionId>,
    ) -> std::result::Result<Option<bool>, facade_support::QueuedWorkRunError> {
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        let mut policy = self.config.policy.clone();
        policy.session_id = Some(session_id.clone());
        self.config
            .store_factory
            .has_claimable_queued_work(
                &SessionStoreCreateRequest {
                    pending_observer_intents: Vec::new(),
                    session_id: session_id.clone(),
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
        Box::pin(self.drive_queued_work(request)).await?;
        Ok(())
    }

    async fn claim_and_run_pending_with_progress(
        &self,
        session_id: Option<&SessionId>,
        reason: &str,
    ) -> std::result::Result<
        facade_support::QueuedWorkRunProgress,
        facade_support::QueuedWorkRunError,
    > {
        Box::pin(self.drive_queued_work(QueuedWorkRunRequest {
            session_id: session_id.cloned(),
            reason: reason.to_string(),
            trace_idle: false,
        }))
        .await
    }
}
