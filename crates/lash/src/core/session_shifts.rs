use super::build_plugin_host;
use crate::support::{
    Arc, DeploymentStore, LashRuntime, LiveReplayStore, PluginFactory, RuntimeEnvironment,
    RuntimeHandle, async_trait,
};
use lash_sansio::SessionId;

pub(crate) struct CoreSessionShiftsConfig {
    /// The core's seat in the recovery leader election (ADR 0109 §1.6).
    pub(super) recovery: Arc<super::recovery::RecoverySlot>,
    pub(super) residents: Arc<super::residents::ResidentSessions>,
    pub(super) shift_owner: lash_core::LeaseOwnerIdentity,
    pub(super) env: RuntimeEnvironment,
    pub(super) protocol_factory: Option<Arc<dyn PluginFactory>>,
    pub(super) plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    pub(super) store_factory: Arc<dyn DeploymentStore>,
    pub(super) live_replay_store: Arc<dyn LiveReplayStore>,
    pub(super) process_lifecycle_available: bool,
}

/// The core's `SessionShifts` (FIG-3600): opens a session's runtime with the
/// core's plugins and runs the kernel's shift on it.
///
/// It is installed on the backend's session-work engine at core build.
pub(crate) struct CoreSessionShifts {
    config: Arc<CoreSessionShiftsConfig>,
    /// The sessions a shift attempt holds open in this process, so its
    /// admissions and runs share one runtime (FIG-3825).
    held: super::held_shifts::HeldShifts,
    /// The core's resolved work ports, bound once the substrate slot exists:
    /// the reconcile pass asks this port for shifts, not the environment's
    /// build-time placeholder.
    substrate_slot: std::sync::OnceLock<std::sync::Weak<super::work_drivers::CoreWorkSlot>>,
    /// What the core administers sessions through, bound with the substrate
    /// slot: the session-delete relay delivers through it (ADR 0109 §4).
    administration: std::sync::OnceLock<super::AdministrationSource>,
    /// Each obligation kind's due-pass lane, across the recovery ticks this
    /// deployment runs (ADR 0109 §1.8). Dropped with the deployment, which
    /// aborts every pass still delivering.
    lanes: lash_core::runtime::shift::RelayLanes,
}

impl CoreSessionShifts {
    pub(crate) fn new(config: Arc<CoreSessionShiftsConfig>) -> Self {
        let lanes = lash_core::runtime::shift::RelayLanes::new(
            Arc::clone(&config.env.core.clock),
            config.env.core.control.recovery_pass,
        );
        Self {
            lanes,
            config,
            held: super::held_shifts::HeldShifts::default(),
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
        Vec<Arc<dyn lash_core::runtime::shift::relay::ObligationRelay>>,
        lash_core::runtime::shift::ObligationRelayUnavailable,
    > {
        let administration = match self.administration.get() {
            Some(source) => source.administration().await,
            None => None,
        };
        lash_core::runtime::shift::obligation_relays(lash_core::runtime::shift::RelayParts {
            tracing: self.config.env.core.tracing.clone(),
            backend: self.config.env.core.backend().clone(),
            sessions: Arc::clone(&self.config.store_factory),
            work: ports.queued_port(),
            scopes: Arc::clone(&self.config.env.core.control.scope_close),
            processes: Some(ports.process.clone()),
            administration,
            trigger_route_restorer: self.config.env.core.control.trigger_route_restorer.clone(),
            clock: Arc::clone(&self.config.env.core.clock),
            policy: self.config.env.core.control.relay_policy(),
            metrics: self.config.env.core.tracing.metrics().clone(),
        })
    }

    /// The core's seat in the recovery leader election.
    pub(crate) fn recovery(&self) -> Arc<super::recovery::RecoverySlot> {
        Arc::clone(&self.config.recovery)
    }

    /// Bind the substrate slot that resolves this core's work ports. The
    /// slot is built after the `SessionShifts` (its setup embeds the `SessionShifts`), so the
    /// binding lands late; a reconcile before it falls back to the
    /// environment's port.
    pub(crate) fn bind_substrate_slot(
        &self,
        slot: std::sync::Weak<super::work_drivers::CoreWorkSlot>,
    ) {
        let _ = self.substrate_slot.set(slot);
    }

    /// The runtime a run of `session_id` runs on.
    async fn shift_runtime(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<ShiftRuntime, OpenFailure> {
        // A session a host holds open is executed on its own runtime, which
        // carries what the host configured on it.
        if let Some(borrow) = self.config.residents.borrow(session_id) {
            return Ok(ShiftRuntime::Resident(borrow));
        }
        let open = Box::pin(self.open_runtime(session_id));
        let Some(held) = self.held.held(session_id) else {
            return open.await.map(ShiftRuntime::Opened);
        };
        let handle = held.runtime(open).await?;
        Ok(ShiftRuntime::Held { handle, held })
    }

    /// Open `session_id`'s runtime from the store with the core's plugins.
    async fn open_runtime(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<RuntimeHandle, OpenFailure> {
        // The engine executes only a session that exists: a missing one is
        // terminal, never a silent create (FIG-4112). Resolution writes no
        // catalog row.
        let store =
            crate::session::resolve_existing_session(&self.config.store_factory, session_id)
                .await
                .map_err(|error| OpenFailure::of_open_read(session_id, error))?;
        // A row with no head recorded no config: the shift is refused with
        // the typed code, never run on defaults (FIG-4553).
        let state = crate::session::load_state_from_store(session_id, &store)
            .await
            .map_err(|error| OpenFailure::of_open_read(session_id, error))?;
        let plugin_host = build_plugin_host(
            self.config.protocol_factory.as_ref(),
            self.config.plugin_factories.as_ref(),
            &self.config.env.core.tracing,
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
        // The shift runs the policy the session recorded; this core states
        // none of its own (FIG-4594).
        let policy = state.effective_policy().clone();
        let runtime = LashRuntime::from_environment(
            &env,
            policy,
            state,
            Some(store),
            self.config.shift_owner.clone(),
        )
        .await
        // Assembly binds the loaded state to its store with one more catalog
        // lookup: its failure is a store read of this open like the two
        // above (FIG-4628).
        .map_err(|error| match error {
            lash_core::SessionError::Plugin(error) => OpenFailure::Terminal(error),
            lash_core::SessionError::Store { source, .. } => {
                OpenFailure::of_store_error(session_id, source)
            }
            error => OpenFailure::Terminal(lash_core::PluginError::Session(error.to_string())),
        })?;
        // The session runs with the config it recorded at creation, its
        // plugin configuration included, unchanged (FIG-4099, FIG-4112,
        // FIG-4379).
        Ok(RuntimeHandle::with_live_replay_store(
            runtime,
            Arc::clone(&self.config.live_replay_store),
        ))
    }
}

/// The runtime a run executes on: the host's open session, borrowed for the
/// run; the one a shift attempt holds open across its runs; or one opened
/// from the store for this run alone.
enum ShiftRuntime {
    Resident(super::residents::ResidentBorrow),
    Held {
        handle: RuntimeHandle,
        held: Arc<super::held_shifts::HeldSession>,
    },
    Opened(RuntimeHandle),
}

impl ShiftRuntime {
    fn handle(&self) -> &RuntimeHandle {
        match self {
            Self::Resident(borrow) => borrow.runtime(),
            Self::Held { handle, .. } | Self::Opened(handle) => handle,
        }
    }

    /// The mark of a run that did not end, on a runtime later runs of the
    /// shift run on too.
    fn unsettled_run(&self) -> Option<&super::held_shifts::UnsettledRun> {
        match self {
            Self::Held { held, .. } => Some(held.unsettled_run()),
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

/// Why a session's runtime did not open for a shift.
enum OpenFailure {
    /// A transient store fault aborted the open; the engine retries it.
    Retry(lash_core::RuntimeError),
    /// The session was deleted or is closing past admission: a journaled
    /// step still has to be emitted for it so the invocation does not
    /// diverge from a `run` command an earlier attempt journaled, and that
    /// step's recorded body answers the retirement (ADR 0104 O1, FIG-3630).
    SessionRetired(lash_core::RuntimeError),
    /// The session's catalog row has no head, so its creation recorded no
    /// config to open with (FIG-4553).
    CreationUnrecorded(lash_core::RuntimeError),
    /// The store answered a read with an error the engine carries under a
    /// terminal code — a refusal that stays typed past the store
    /// ([`StoreRefusal`](lash_core::store::StoreRefusal)), or corrupt stored
    /// data: the shift is refused with that code and cause, which a sender
    /// reads (FIG-4597, FIG-4628).
    StoreRefused(lash_core::RuntimeError),
    Terminal(lash_core::PluginError),
}

impl OpenFailure {
    /// Why a store read of the open failed: the catalog lookup, the recorded
    /// state's load, or the lookup runtime assembly binds the state with.
    /// Each is classified here and nowhere else.
    ///
    /// - A retired session keeps its recorded retirement.
    /// - A fault of the storage substrate
    ///   ([`StoreError::is_transient`](lash_core::StoreError::is_transient))
    ///   is retried.
    /// - Every other answer is the store's on each attempt, so the shift is
    ///   refused: under the terminal code the engine carries the error by,
    ///   with its typed cause, or naming a refusal that has no such code.
    fn of_store_error(session_id: &SessionId, error: lash_core::StoreError) -> Self {
        match error {
            error @ (lash_core::StoreError::SessionDeleted { .. }
            | lash_core::StoreError::SessionClosing { .. }) => {
                Self::SessionRetired(session_retired_error(session_id, error))
            }
            lash_core::StoreError::Contended => Self::Retry(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::StoreCommitContended,
                "the session's runtime is contended; the shift is retried",
            )),
            error if error.is_transient() => Self::Retry(error.runtime_error()),
            error if error.runtime_code().is_terminal() => {
                Self::StoreRefused(error.runtime_error())
            }
            error => Self::Terminal(lash_core::PluginError::from(error)),
        }
    }

    /// Why resolving the session or loading its recorded state failed.
    fn of_open_read(session_id: &SessionId, error: crate::EmbedError) -> Self {
        match error {
            crate::EmbedError::Store(error) => Self::of_store_error(session_id, error),
            error @ crate::EmbedError::SessionCreationUnrecorded { .. } => {
                Self::CreationUnrecorded(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::SessionCreationUnrecorded,
                    error.to_string(),
                ))
            }
            error => Self::Terminal(lash_core::PluginError::Session(error.to_string())),
        }
    }

    fn into_abort(self) -> lash_core::engine::ShiftAbort {
        match self {
            Self::Retry(error) => lash_core::engine::ShiftAbort::Retry(error),
            Self::SessionRetired(error)
            | Self::CreationUnrecorded(error)
            | Self::StoreRefused(error) => lash_core::engine::ShiftAbort::Refused(error),
            // A plugin's own typed store refusal keeps its code and cause;
            // every other plugin failure is named under the session seam's.
            Self::Terminal(error @ lash_core::PluginError::StoreRefusal(_)) => {
                lash_core::engine::ShiftAbort::Refused(
                    lash_core::RuntimeEffectControllerError::from(error).into_runtime_error(),
                )
            }
            Self::Terminal(error) => {
                lash_core::engine::ShiftAbort::Refused(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::PluginSessionManager,
                    error.to_string(),
                ))
            }
        }
    }
}

#[async_trait]
impl lash_core::SessionShifts for CoreSessionShifts {
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
        // The `SessionShifts` is installed by the core that bound the generation; a
        // tick that finds none has nothing to reconcile against.
        let Ok(generation) = backend.build_generation().cloned() else {
            return Ok(cursor.clone());
        };
        let processes = self
            .config
            .env
            .process_registry()
            .zip(process_port.as_ref())
            .map(
                |(registry, port)| lash_core::runtime::shift::ReconcileProcesses {
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
        let report = lash_core::runtime::shift::reconcile_once(
            &lash_core::runtime::shift::ReconcileParts {
                metrics: self.config.env.core.tracing.metrics(),
                sessions: self.config.store_factory.as_ref(),
                work: work.as_ref(),
                scopes: self.config.env.core.control.scope_close.as_ref(),
                processes,
                clock: self.config.env.core.clock.as_ref(),
                duties,
                relays: &relays,
                lanes: &self.lanes,
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

    fn hold_shift(&self, session: &SessionId) -> lash_core::engine::ShiftHold {
        lash_core::engine::ShiftHold::new(self.held.hold(session))
    }

    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &lash_core::engine::ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
        draining: Option<&lash_core::engine::BuildGeneration>,
    ) -> std::result::Result<lash_core::engine::AdmitVerdict, lash_core::engine::ShiftAbort> {
        // The admission runs on no runtime of the session, whether a host
        // holds it open, this shift holds it, or nothing does: its recorded
        // step reads only the session's store. A run keeps its runtime's
        // writer for as long as it runs, and a shift that replays beside a
        // run it called must reach the recorded call it waits for that run
        // on, so an admission never waits for a writer (FIG-4729, FIG-4755).
        let store = match crate::session::resolve_existing_session(
            &self.config.store_factory,
            &request.session,
        )
        .await
        {
            Ok(store) => store,
            Err(error) => match OpenFailure::of_open_read(&request.session, error) {
                // A session that is already deleted still owes the journal
                // the recorded step at this position: an attempt that
                // stopped short of it would diverge from the `run` command
                // an earlier attempt journaled, and the step's recorded body
                // answers the same retirement every redrive replays
                // (ADR 0104 O1, FIG-3630).
                OpenFailure::SessionRetired(_) => {
                    return lash_core::shift::admit_shift_retired(
                        &controller,
                        request,
                        admitting_generation,
                        ordinal,
                        Arc::clone(&self.config.store_factory),
                    )
                    .await;
                }
                failure => return Err(failure.into_abort()),
            },
        };
        lash_core::shift::admit_shift_on_store(
            &self.config.env.core,
            store,
            &controller,
            request,
            admitting_generation,
            ordinal,
            draining,
        )
        .await
    }

    async fn execute_run(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        admitted: lash_core::engine::Admitted,
    ) -> lash_core::engine::RunEnd {
        let runtime = match self.shift_runtime(admitted.session()).await {
            Ok(runtime) => runtime,
            // A run whose session is already deleted — or closed past
            // admission — still owes the journal its start marker and seal:
            // an attempt that stopped short of them would diverge from what
            // an earlier attempt of the run journaled, and the seal's
            // recorded body answers the same retirement every redrive
            // replays (ADR 0104 O1, FIG-3881).
            Err(OpenFailure::SessionRetired(_)) => {
                return lash_core::engine::RunEnd::owing_nothing(
                    lash_core::shift::execute_admitted_run_retired(&controller, admitted).await,
                );
            }
            Err(failure) => {
                return lash_core::engine::RunEnd::owing_nothing(Err(failure.into_abort()));
            }
        };
        crate::turn::execute_admitted_run_observed(
            runtime.handle(),
            &self.config.env.core.backend().binding_identity(),
            &controller,
            admitted,
            runtime.unsettled_run(),
        )
        .await
    }

    /// The close runs on no runtime of the session: the host config's
    /// catalog, scope owner and obligation ledger are all it reads, so it
    /// never waits on, or holds, the writer the session's next run executes on.
    async fn close_run(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        session: &SessionId,
        run: &lash_core::TurnId,
    ) -> std::result::Result<(), lash_core::engine::ShiftAbort> {
        lash_core::shift::close_admitted_run(&self.config.env.core, &controller, session, run).await
    }
}
