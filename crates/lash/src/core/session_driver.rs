use super::build_plugin_host;
use crate::support::{
    Arc, DeploymentStore, LashRuntime, LiveReplayStore, PluginFactory, RuntimeEnvironment,
    RuntimeHandle, SessionPolicy, SessionRelation, SessionStoreCreateRequest, async_trait,
};
use lash_sansio::SessionId;

pub(crate) struct CoreSessionDriverConfig {
    /// The core's seat in the recovery leader election (ADR 0109 §1.6).
    pub(super) recovery: Arc<super::recovery::RecoverySlot>,
    pub(super) residents: Arc<super::residents::ResidentSessions>,
    pub(super) drive_owner: lash_core::LeaseOwnerIdentity,
    pub(super) env: RuntimeEnvironment,
    pub(super) policy: SessionPolicy,
    pub(super) protocol_factory: Option<Arc<dyn PluginFactory>>,
    pub(super) plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    pub(super) store_factory: Arc<dyn DeploymentStore>,
    pub(super) live_replay_store: Arc<dyn LiveReplayStore>,
    pub(super) process_lifecycle_available: bool,
}

/// The core's session driver (FIG-3600): opens a session's runtime with the
/// core's plugins and runs the kernel's drive on it.
///
/// It is installed on the backend's session-work engine at core build.
pub(crate) struct CoreSessionDriver {
    config: Arc<CoreSessionDriverConfig>,
    /// The sessions a drive attempt holds open in this process, so its
    /// admissions and roots share one runtime (FIG-3825).
    held: super::held_drives::HeldDrives,
    /// The core's resolved work ports, bound once the substrate slot exists:
    /// the reconcile pass asks this port for drives, not the environment's
    /// build-time placeholder.
    substrate_slot: std::sync::OnceLock<std::sync::Weak<super::work_drivers::CoreWorkSlot>>,
    /// What the core administers sessions through, bound with the substrate
    /// slot: the session-delete relay delivers through it (ADR 0109 §4).
    administration: std::sync::OnceLock<super::AdministrationSource>,
}

impl CoreSessionDriver {
    pub(crate) fn new(config: Arc<CoreSessionDriverConfig>) -> Self {
        Self {
            config,
            held: super::held_drives::HeldDrives::default(),
            substrate_slot: std::sync::OnceLock::new(),
            administration: std::sync::OnceLock::new(),
        }
    }

    /// Bind what the core administers sessions through.
    pub(crate) fn bind_administration(&self, source: super::AdministrationSource) {
        let _ = self.administration.set(source);
    }

    /// Every obligation kind's relay this tick claims due rows for, one per
    /// kind the store set arms (ADR 0109 §1.4): lash-core assembles them
    /// from the core's resolved ports, never per host.
    async fn relays(
        &self,
        ports: &super::work_drivers::ResolvedPorts,
    ) -> std::result::Result<
        Vec<Arc<dyn lash_core::runtime::drive::relay::ObligationRelay>>,
        lash_core::runtime::drive::ObligationRelayUnavailable,
    > {
        let administration = match self.administration.get() {
            Some(source) => source.administration().await,
            None => None,
        };
        lash_core::runtime::drive::obligation_relays(lash_core::runtime::drive::RelayParts {
            backend: self.config.env.core.backend().clone(),
            sessions: Arc::clone(&self.config.store_factory),
            work: ports.queued_port(),
            scopes: Arc::clone(&self.config.env.core.control.scope_close),
            processes: Some(ports.process.clone()),
            administration,
            clock: Arc::clone(&self.config.env.core.clock),
        })
    }

    /// The core's seat in the recovery leader election.
    pub(crate) fn recovery(&self) -> Arc<super::recovery::RecoverySlot> {
        Arc::clone(&self.config.recovery)
    }

    /// Bind the substrate slot that resolves this core's work ports. The
    /// slot is built after the driver (its setup embeds the driver), so the
    /// binding lands late; a reconcile before it falls back to the
    /// environment's port.
    pub(crate) fn bind_substrate_slot(
        &self,
        slot: std::sync::Weak<super::work_drivers::CoreWorkSlot>,
    ) {
        let _ = self.substrate_slot.set(slot);
    }

    /// The runtime a drive step of `session_id` runs on.
    async fn drive_runtime(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<DriveRuntime, OpenFailure> {
        // A session a host holds open is driven on its own runtime, which
        // carries what the host configured on it.
        if let Some(borrow) = self.config.residents.borrow(session_id) {
            return Ok(DriveRuntime::Resident(borrow));
        }
        let open = Box::pin(self.open_runtime(session_id));
        let Some(held) = self.held.held(session_id) else {
            return open.await.map(DriveRuntime::Opened);
        };
        let handle = held.runtime(open).await?;
        Ok(DriveRuntime::Held { handle, held })
    }

    /// Open `session_id`'s runtime from the store with the core's plugins.
    async fn open_runtime(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<RuntimeHandle, OpenFailure> {
        let mut policy = self.config.policy.clone();
        policy.session_id = Some(session_id.clone());
        let store = lash_core::runtime::admit_session_view(
            &self.config.store_factory,
            &SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: SessionRelation::default(),
                policy: policy.clone(),
            },
        )
        .await
        .map_err(|error| match error {
            error @ (lash_core::StoreError::SessionDeleted { .. }
            | lash_core::StoreError::SessionClosing { .. }) => {
                OpenFailure::SessionRetired(session_retired_error(session_id, error))
            }
            error => OpenFailure::Terminal(lash_core::PluginError::Session(error.to_string())),
        })?;
        let state = match crate::session::load_state_from_store(session_id, &policy, &store).await {
            Ok(state) => state,
            Err(crate::EmbedError::Store(lash_core::StoreError::Contended)) => {
                return Err(OpenFailure::Contended);
            }
            Err(crate::EmbedError::Store(
                error @ (lash_core::StoreError::SessionDeleted { .. }
                | lash_core::StoreError::SessionClosing { .. }),
            )) => {
                return Err(OpenFailure::SessionRetired(session_retired_error(
                    session_id, error,
                )));
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
        let is_root_session = state.authority.subagent.is_none();
        let mut runtime = LashRuntime::from_environment(
            &env,
            policy,
            state,
            Some(store),
            self.config.drive_owner.clone(),
        )
        .await
        .map_err(|error| {
            OpenFailure::Terminal(match error {
                lash_core::SessionError::Plugin(error) => error,
                error => lash_core::PluginError::Session(error.to_string()),
            })
        })?;
        // The protocol applies and pins its per-session options on every
        // open, as a host's open does: a session the engine opens first (a
        // send to a session no host committed yet) records them with its
        // first commit, so a later open rematerializes it.
        runtime
            .configure_protocol_on_materialize(
                &lash_core::PluginOptions::default(),
                is_root_session,
            )
            .map_err(OpenFailure::Terminal)?;
        Ok(RuntimeHandle::with_live_replay_store(
            runtime,
            Arc::clone(&self.config.live_replay_store),
        ))
    }
}

/// The runtime a drive step runs on: the host's open session, borrowed for
/// the step; the one a drive attempt holds open across its steps; or one
/// opened from the store for this step alone.
enum DriveRuntime {
    Resident(super::residents::ResidentBorrow),
    Held {
        handle: RuntimeHandle,
        held: Arc<super::held_drives::HeldSession>,
    },
    Opened(RuntimeHandle),
}

impl DriveRuntime {
    fn handle(&self) -> &RuntimeHandle {
        match self {
            Self::Resident(borrow) => borrow.runtime(),
            Self::Held { handle, .. } | Self::Opened(handle) => handle,
        }
    }

    /// The mark of a root that did not end, on a runtime later roots of the
    /// drive run on too.
    fn unsettled_root(&self) -> Option<&super::held_drives::UnsettledRoot> {
        match self {
            Self::Held { held, .. } => Some(held.unsettled_root()),
            Self::Resident(_) | Self::Opened(_) => None,
        }
    }
}

/// The runtime error a journaled step records for a session whose close or
/// tombstone already committed: the typed retirement refusal every replay
/// of it decodes (FIG-3630).
fn session_retired_error(
    session_id: &SessionId,
    error: lash_core::StoreError,
) -> lash_core::RuntimeError {
    lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::SessionDeleted,
        error.to_string(),
    )
    .with_cause(lash_core::RuntimeErrorCause::SessionDeleted {
        session_id: session_id.clone(),
    })
}

/// Why a session's runtime did not open for a drive.
enum OpenFailure {
    /// Another writer holds the session; the drive is retried.
    Contended,
    /// The session was deleted or is closing past admission: a journaled
    /// step still has to be emitted for it so the invocation does not
    /// diverge from a `run` command an earlier attempt journaled, and that
    /// step's recorded body answers the retirement (ADR 0104 O1, FIG-3630).
    SessionRetired(lash_core::RuntimeError),
    Terminal(lash_core::PluginError),
}

impl OpenFailure {
    fn into_abort(self) -> lash_core::engine::DriveAbort {
        match self {
            Self::Contended => lash_core::engine::DriveAbort::Retry(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::StoreCommitContended,
                "the session's runtime is contended; the drive is retried",
            )),
            Self::SessionRetired(error) => lash_core::engine::DriveAbort::Refused(error),
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
impl lash_core::SessionDriver for CoreSessionDriver {
    fn owns_reconciliation(&self) -> bool {
        true
    }

    async fn reconcile(
        &self,
        cursor: &lash_core::engine::ReconcileCursor,
        page: std::num::NonZeroUsize,
    ) -> std::result::Result<lash_core::engine::ReconcileCursor, lash_core::StoreError> {
        // The environment's build-time queued port is a placeholder; the
        // relays and arms ask the ports the substrate resolved — the engine
        // the core's sends deliver to. A tick before the slot is bound, or
        // after the core is gone, has nothing to deliver through: the next
        // one runs.
        let Some(slot) = self.substrate_slot.get().and_then(std::sync::Weak::upgrade) else {
            return Ok(cursor.clone());
        };
        let ports = slot.ports().await;
        let work = ports.queued_port();
        let process_port = self.config.env.process_work();
        let backend = self.config.env.core.backend();
        let drain = backend.generation_drain();
        let generation = backend.build_generation().clone();
        let processes = self
            .config
            .env
            .process_registry()
            .zip(process_port.as_ref())
            .map(
                |(registry, port)| lash_core::runtime::drive::ReconcileProcesses {
                    registry: registry.as_ref(),
                    port: port.as_ref(),
                    drain: drain.as_ref(),
                    generation: &generation,
                },
            );
        // Which recovery duties this deployment runs this tick (ADR 0109
        // §1.7).
        let duties = self.config.recovery.duties().await;
        let relays = match self.relays(&ports).await {
            Ok(relays) => relays,
            Err(error) => {
                tracing::error!(error = %error, "the reconcile tick cannot run every obligation relay");
                return Ok(cursor.clone());
            }
        };
        let report = lash_core::runtime::drive::reconcile_once(
            &lash_core::runtime::drive::ReconcileParts {
                sessions: self.config.store_factory.as_ref(),
                work: work.as_ref(),
                scopes: self.config.env.core.control.scope_close.as_ref(),
                processes,
                clock: self.config.env.core.clock.as_ref(),
                duties,
                relays: &relays,
            },
            cursor,
            page,
        )
        .await;
        for failure in &report.failures {
            tracing::warn!(arm = ?failure.arm, error = %failure.error, "reconcile arm failed; a later pass retries it");
        }
        Ok(report.next)
    }

    fn hold_drive(&self, session: &SessionId) -> lash_core::engine::DriveHold {
        lash_core::engine::DriveHold::new(self.held.hold(session))
    }

    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &lash_core::engine::DriveRequest,
        ordinal: u32,
    ) -> std::result::Result<lash_core::engine::AdmitVerdict, lash_core::engine::DriveAbort> {
        let runtime = match self.drive_runtime(&request.session).await {
            Ok(runtime) => runtime,
            // A session that is already deleted — or closed past admission —
            // still owes the journal the recorded step at this position: an
            // attempt that stopped short of it would diverge from the `run`
            // command an earlier attempt journaled, and the step's recorded
            // body answers the same retirement every redrive replays
            // (ADR 0104 O1, FIG-3630).
            Err(OpenFailure::SessionRetired(_)) => {
                return lash_core::drive::admit_drive_retired(
                    &controller,
                    request,
                    ordinal,
                    Arc::clone(&self.config.store_factory),
                )
                .await;
            }
            Err(failure) => return Err(failure.into_abort()),
        };
        crate::turn::admit_drive_observed(runtime.handle(), &controller, request, ordinal).await
    }

    async fn run_root(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        admitted: lash_core::engine::Admitted,
    ) -> lash_core::engine::RootRunEnd {
        let runtime = match self.drive_runtime(admitted.session()).await {
            Ok(runtime) => runtime,
            // A root whose session is already deleted — or closed past
            // admission — still owes the journal its start marker and seal:
            // an attempt that stopped short of them would diverge from what
            // an earlier attempt of the run journaled, and the seal's
            // recorded body answers the same retirement every redrive
            // replays (ADR 0104 O1, FIG-3881).
            Err(OpenFailure::SessionRetired(_)) => {
                return lash_core::engine::RootRunEnd::owing_nothing(
                    lash_core::drive::run_admitted_root_retired(&controller, admitted).await,
                );
            }
            Err(failure) => {
                return lash_core::engine::RootRunEnd::owing_nothing(Err(failure.into_abort()));
            }
        };
        crate::turn::run_admitted_root_observed(
            runtime.handle(),
            &self.config.env.core.backend().binding_identity(),
            &controller,
            admitted,
            runtime.unsettled_root(),
        )
        .await
    }

    /// The close runs on no runtime of the session: the host config's
    /// catalog, scope owner and obligation ledger are all it reads, so it
    /// never waits on, or holds, the writer the session's next root runs on.
    async fn close_root(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        session: &SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<(), lash_core::engine::DriveAbort> {
        lash_core::drive::close_admitted_root(&self.config.env.core, &controller, session, root)
            .await
    }
}
