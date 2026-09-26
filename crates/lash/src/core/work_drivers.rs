use super::queued_work::NativeQueuedWorkRunHandle;
#[cfg(test)]
use crate::support::DurableProcessWorkerConfig;
use crate::support::{
    Arc, DurableProcessWorker, NativeProcessWork, NativeSubstrateConfig, NoSessionWork,
    ProcessRegistry, ProcessWorkSubstrate, ProcessWorkWiring, SessionStoreFactory,
    SessionWorkEngine, WorkerProcessWork, WorkerSlotSupplier, async_trait,
};
use lash_core::facade_support;
use tokio_util::sync::CancellationToken;

/// How a [`LashCore`] resolves its process-work port, decided at `build()`
/// and shared across clones.
pub(super) enum ProcessPortSetup {
    /// Lazily construct the native process-work port over the backend's
    /// registry on first `session().open()`; the worker rebuilds a session
    /// runtime per process through the backend's catalog.
    NativeDefault {
        config: Box<super::NativeProcessWorkerSetup>,
        watched: facade_support::WatchedRegistry,
    },
    /// The backend supplies its own process work.
    External { wiring: ProcessWorkWiring },
}

#[derive(Clone)]
pub(super) enum ProcessWorkSelection {
    Native(Arc<dyn ProcessRegistry>),
    External(ProcessWorkWiring),
}

impl ProcessWorkSelection {
    pub(super) fn resolve(
        self,
        clock: Arc<dyn lash_core::Clock>,
        sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
    ) -> ProcessWorkSource {
        match self {
            Self::Native(registry) => {
                let registry = registry.with_runtime_clock(clock).unwrap_or(registry);
                ProcessWorkSource::Native(facade_support::watch_process_registry_with_sink(
                    registry, sink,
                ))
            }
            Self::External(wiring) => ProcessWorkSource::External(wiring),
        }
    }
}

#[derive(Clone)]
pub(super) enum ProcessWorkSource {
    Native(facade_support::WatchedRegistry),
    External(ProcessWorkWiring),
}

impl ProcessWorkSource {
    /// The registry the core reads and binds: the watched native registry,
    /// or the one the backend's own process work is wired over.
    pub(super) fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        match self {
            Self::Native(watched) => Arc::clone(watched.registry()),
            Self::External(wiring) => Arc::clone(wiring.registry()),
        }
    }
}

/// Whether the core runs the backend's queued-work driver.
#[derive(Clone, Copy)]
pub(super) enum QueuedWorkSource {
    /// The backend's own driver, or the in-process driver when it has none.
    Backend,
    /// No driver: the host runs every queued turn itself.
    Disabled,
}

pub(super) enum QueuedPortSetup {
    /// The host turned the backend's own engine off: nothing would drive an
    /// accepted input, so a send is refused before it accepts anything.
    Disabled,
    Native {
        driver: Arc<NativeQueuedWorkRunHandle>,
        slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
        execution_concurrency: usize,
    },
    External {
        port: Arc<dyn SessionWorkEngine>,
    },
}

pub(super) struct WakeDeliveryDriverSetup {
    pub(super) registry: Arc<dyn ProcessRegistry>,
    pub(super) factory: Arc<dyn SessionStoreFactory>,
    pub(super) clock: Arc<dyn lash_core::Clock>,
    pub(super) delivery_policy: lash_core::DeliveryPolicy,
}

pub(super) struct NativeSubstrateSetup {
    pub(super) config: NativeSubstrateConfig,
    pub(super) process: ProcessPortSetup,
    pub(super) queued: QueuedPortSetup,
    pub(super) wake: WakeDeliveryDriverSetup,
}

#[derive(Clone)]
pub(crate) struct ResolvedPorts {
    pub(crate) process: ProcessWorkWiring,
    pub(crate) queued: Arc<ResolvedQueuedWork>,
    pub(crate) drive_process_on_open: bool,
}

impl ResolvedPorts {
    pub(crate) fn queued_port(&self) -> Arc<dyn SessionWorkEngine> {
        self.queued.clone()
    }
}

/// Stops the core's in-process drives once its host holds nothing of the
/// core: no core, session, durable session or send handle. A host that has
/// let go of all of them has stopped this worker, so a drive still running
/// on one of its sessions stops with it instead of holding the session's
/// lane on its own; a peer takes the lane over as from any stopped worker.
///
/// Drives hold runtimes, and runtimes hold the engine, so the engine alone
/// never learns that its host is gone: its shutdown is a child of this token.
pub(crate) struct DriveLifetime(CancellationToken);

impl DriveLifetime {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(CancellationToken::new()))
    }

    pub(super) fn token(&self) -> CancellationToken {
        self.0.clone()
    }
}

impl Drop for DriveLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// The core's queued work as a host-held handle carries it: the engine, and
/// the lifetime that keeps its in-process drives running.
#[derive(Clone)]
pub(crate) struct HeldWork {
    work: Arc<ResolvedQueuedWork>,
    _drives: Arc<DriveLifetime>,
}

impl HeldWork {
    pub(crate) fn new(work: Arc<ResolvedQueuedWork>, drives: Arc<DriveLifetime>) -> Self {
        Self {
            work,
            _drives: drives,
        }
    }

    /// The engine alone, for what a runtime holds.
    pub(crate) fn engine(&self) -> Arc<ResolvedQueuedWork> {
        Arc::clone(&self.work)
    }
}

impl std::ops::Deref for HeldWork {
    type Target = ResolvedQueuedWork;

    fn deref(&self) -> &ResolvedQueuedWork {
        &self.work
    }
}

pub(crate) struct ResolvedQueuedWork {
    port: Arc<dyn SessionWorkEngine>,
    wake: std::sync::Mutex<Option<facade_support::WakeDeliveryDriver>>,
    /// No engine would drive an accepted input (FIG-3600 S5b).
    refuses_sends: bool,
}

impl ResolvedQueuedWork {
    fn new(port: Arc<dyn SessionWorkEngine>, refuses_sends: bool) -> Self {
        Self {
            port,
            wake: std::sync::Mutex::new(None),
            refuses_sends,
        }
    }

    /// Whether a send must be refused before acceptance: no engine and no
    /// in-process drive would ever run what it accepted.
    pub(crate) fn refuses_sends(&self) -> bool {
        self.refuses_sends
    }

    fn install_wake(&self, wake: facade_support::WakeDeliveryDriver) {
        *self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(wake);
    }

    pub(crate) async fn drive_wake(
        &self,
    ) -> std::result::Result<facade_support::WakeDeliveryDriveReport, lash_core::PluginError> {
        let wake = self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(wake) = wake else {
            return Err(lash_core::PluginError::Session(
                "wake delivery driver is unavailable in this runtime".to_string(),
            ));
        };
        wake.drive_pending().await
    }
}

impl Drop for ResolvedQueuedWork {
    fn drop(&mut self) {
        if let Some(wake) = self
            .wake
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            wake.request_shutdown();
        }
    }
}

#[async_trait]
impl SessionWorkEngine for ResolvedQueuedWork {
    fn schedule_drive(
        &self,
        session: &lash_core::SessionId,
        request: lash_core::engine::DriveRequestId,
    ) {
        self.port.schedule_drive(session, request);
    }

    fn install_session_driver(
        &self,
        driver: Arc<dyn lash_core::SessionDriver>,
    ) -> Arc<dyn lash_core::SessionDriver> {
        self.port.install_session_driver(driver)
    }

    async fn await_drive(
        &self,
        session: &lash_core::SessionId,
        request: &lash_core::engine::DriveRequestId,
    ) -> std::result::Result<lash_core::engine::DriveOutcome, lash_core::engine::DriveAbort> {
        self.port.await_drive(session, request).await
    }

    async fn session_work_in_flight(&self, session: &lash_core::SessionId) -> bool {
        self.port.session_work_in_flight(session).await
    }
}

/// Shared, lazily-initialized host-work state for a [`LashCore`].
///
/// The once-guard ([`tokio::sync::OnceCell`]) constructs native ports exactly
/// once across `LashCore` clones, on the first `session().open()` or admin path
/// that needs them.
pub(crate) struct NativeSubstrateSlot {
    pub(super) setup: NativeSubstrateSetup,
    drivers: tokio::sync::OnceCell<ResolvedPorts>,
    phase_probe_slot: Option<lash_core::runtime::RuntimeTurnPhaseProbeSlot>,
    /// The core's [`DriveLifetime`]: the native engine shuts down with it.
    drives_shutdown: CancellationToken,
}

impl NativeSubstrateSlot {
    pub(super) fn new(setup: NativeSubstrateSetup, drives: &DriveLifetime) -> Self {
        let phase_probe_slot = match &setup.process {
            ProcessPortSetup::NativeDefault { config, .. } => {
                Some(config.turn_phase_probe_slot.clone())
            }
            ProcessPortSetup::External { .. } => None,
        };
        Self {
            setup,
            drivers: tokio::sync::OnceCell::new(),
            phase_probe_slot,
            drives_shutdown: drives.token(),
        }
    }

    /// Idempotent: the once-guard ensures native ports are constructed once.
    #[expect(
        clippy::expect_used,
        reason = "every value assembled here was validated when the setup was built: \
                  the builder refuses an invalid work cadence, execution concurrency \
                  or substrate config before it can reach this resolution"
    )]
    pub(crate) async fn ports(&self) -> ResolvedPorts {
        self.drivers
            .get_or_init(|| async {
                let refuses_sends = matches!(self.setup.queued, QueuedPortSetup::Disabled);
                let queued_port: Arc<dyn SessionWorkEngine> = match &self.setup.queued {
                    QueuedPortSetup::Disabled => Arc::new(NoSessionWork::new()),
                    QueuedPortSetup::External { port } => Arc::clone(port),
                    QueuedPortSetup::Native {
                        driver,
                        slot_supplier,
                        execution_concurrency,
                    } => {
                        let run_handle = Arc::clone(driver);
                        let work_cadence = self.setup.config.work_cadence.clone();
                        let engine = match slot_supplier {
                            Some(slot_supplier) => {
                                facade_support::native_queued_work_with_worker_slot_supplier_and_work_cadence(
                                    run_handle.clone(),
                                    Arc::clone(slot_supplier),
                                    work_cadence,
                                    self.drives_shutdown.clone(),
                                )
                                .expect("native work cadence was validated at build")
                            }
                            None => facade_support::native_queued_work_with_execution_concurrency_and_work_cadence(
                                run_handle,
                                *execution_concurrency,
                                work_cadence,
                                self.drives_shutdown.clone(),
                            )
                            .expect("queued-work concurrency was validated at build"),
                        };
                        // The run handle is also the driver: installing it
                        // starts the engine's reconcile tick.
                        engine.install_session_driver(driver.clone());
                        Arc::new(engine)
                    }
                };
                let (process, drive_process_on_open) = match &self.setup.process {
                    ProcessPortSetup::External { wiring } => (
                        wiring
                            .clone()
                            .with_work_cadence(self.setup.config.work_cadence.clone())
                            .expect("native substrate config was validated at build"),
                        false,
                    ),
                    ProcessPortSetup::NativeDefault { config, watched } => {
                        // The worker only forwards notifications through this port;
                        // the outer dispatcher remains the sole native-lane owner.
                        let config = config
                            .build(Arc::clone(&queued_port))
                            .expect("native process-worker assembly was validated at build");
                        let watched = watched.clone();
                        let worker = DurableProcessWorker::new(config)
                            .expect("native substrate config was validated at build");
                        let port: Arc<dyn ProcessWorkSubstrate> =
                            Arc::new(NativeProcessWork::new(&watched, worker));
                        let wiring = ProcessWorkWiring::new(watched, port)
                            .with_work_cadence(self.setup.config.work_cadence.clone())
                            .expect("native substrate config was validated at build");
                        (wiring, true)
                    }
                };
                let queued = Arc::new(ResolvedQueuedWork::new(queued_port, refuses_sends));
                let setup = &self.setup.wake;
                let queued_for_wake: Arc<dyn SessionWorkEngine> = queued.clone();
                let wake = facade_support::wake_delivery_driver_with_work_cadence(
                    Arc::clone(&setup.registry),
                    Arc::clone(&setup.factory),
                    queued_for_wake,
                    Arc::clone(&setup.clock),
                    setup.delivery_policy,
                    self.setup.config.work_cadence.clone(),
                )
                .expect("native work cadence was validated at build");
                queued.install_wake(wake);
                ResolvedPorts {
                    process,
                    queued,
                    drive_process_on_open,
                }
            })
            .await
            .clone()
    }

    pub(crate) fn phase_probe_slot(&self) -> Option<lash_core::runtime::RuntimeTurnPhaseProbeSlot> {
        self.phase_probe_slot.clone()
    }

    #[cfg(test)]
    pub(crate) fn process_worker_config(&self) -> Option<DurableProcessWorkerConfig> {
        match &self.setup.process {
            ProcessPortSetup::NativeDefault { config, .. } => Some(
                config
                    .build(Arc::new(NoSessionWork::new()))
                    .expect("native process-worker assembly was validated at build"),
            ),
            ProcessPortSetup::External { .. } => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn native_process_change_hub(&self) -> Option<facade_support::ProcessChangeHub> {
        match &self.setup.process {
            ProcessPortSetup::NativeDefault { watched, .. } => Some(watched.hub().clone()),
            ProcessPortSetup::External { .. } => None,
        }
    }

    pub(super) fn configured_worker_process_work(&self) -> WorkerProcessWork {
        match &self.setup.process {
            ProcessPortSetup::NativeDefault { watched, .. } => {
                WorkerProcessWork::SelfNative(watched.clone())
            }
            ProcessPortSetup::External { wiring } => WorkerProcessWork::External(wiring.clone()),
        }
    }
}
